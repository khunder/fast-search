//! Reading a filesystem's own tables straight from a block device or disk
//! image, without mounting it: ext2/3/4 inode tables and directories, and
//! the NTFS master file table. This is how partitions that are not
//! mounted get indexed.
//!
//! Everything read here comes from disk and may be damaged or hostile, so
//! all offsets are bounds-checked and loops are capped; a bad structure
//! is skipped, never trusted.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::index::{FLAG_DIR, FLAG_SYMLINK, Index, NO_PARENT};

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u16le(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 2).map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as u64)
}

fn u32le(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 4).map_or(0, |v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]) as u64)
}

fn u64le(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |v| u64::from_le_bytes(v.try_into().unwrap()))
}

/// One name found on disk: which object it names and in which directory.
/// The name itself is in [`Names::arena`].
struct Found {
    parent: u64,
    object: u64,
    start: u32,
    len: u16,
}

/// Every name found on disk, kept in one buffer to spare millions of
/// small allocations.
#[derive(Default)]
struct Names {
    found: Vec<Found>,
    arena: Vec<u8>,
}

impl Names {
    fn add(&mut self, parent: u64, object: u64, name: &[u8]) {
        let (start, len) = (self.arena.len(), name.len());
        if len > u16::MAX as usize || start + len > u32::MAX as usize {
            return; // not a real name, or an absurd number of them
        }
        self.arena.extend_from_slice(name);
        self.found.push(Found { parent, object, start: start as u32, len: len as u16 });
    }

    fn name(&self, found: &Found) -> &[u8] {
        &self.arena[found.start as usize..found.start as usize + found.len as usize]
    }

    /// Groups the names by directory, for [`Names::children`].
    fn sort(&mut self) {
        self.found.sort_unstable_by_key(|f| f.parent);
    }

    /// The names found in directory `parent` (after [`Names::sort`]).
    fn children(&self, parent: u64) -> &[Found] {
        let lo = self.found.partition_point(|f| f.parent < parent);
        let hi = self.found.partition_point(|f| f.parent <= parent);
        &self.found[lo..hi]
    }
}

/// What we keep about an object (an inode or MFT record).
#[derive(Clone, Copy)]
struct Meta {
    flags: u8,
    /// The Windows hidden attribute.
    hidden: bool,
    size: u64,
    mtime: i64,
}

/// Finds an object's [`Meta`] by number.
trait Objects {
    fn meta(&self, object: u64) -> Option<Meta>;
}

impl Objects for HashMap<u64, Meta> {
    fn meta(&self, object: u64) -> Option<Meta> {
        self.get(&object).copied()
    }
}

/// What to leave out while assembling: the same rules as the folder walk.
pub(crate) struct Prune<'a> {
    /// Folders listed but not entered (absolute paths, as index bytes).
    pub excludes: &'a [Vec<u8>],
    /// Leave out names starting with a dot, and hidden files.
    pub skip_hidden: bool,
}

/// Adds the names found on disk to `index`, under a root entry named
/// `root_name` standing for object `root`. Entries are added parent
/// first, as the index requires; names whose directory is unreachable
/// from the root are left out. `names` must be sorted.
fn assemble(index: &mut Index, root_name: &[u8], root: u64, names: &Names, meta: &impl Objects, prune: &Prune) {
    let root_meta = meta.meta(root).unwrap_or(Meta { flags: FLAG_DIR, hidden: false, size: 0, mtime: 0 });
    let root_id = index.push(root_name, NO_PARENT, FLAG_DIR, 0, root_meta.mtime);
    // Each directory is visited once, so a damaged tree with a loop ends.
    let mut visited = std::collections::HashSet::from([root]);
    let mut queue = std::collections::VecDeque::from([(root, root_id, root_name.to_vec())]);
    while let Some((object, id, path)) = queue.pop_front() {
        for found in names.children(object) {
            let Some(m) = meta.meta(found.object) else { continue };
            let name = names.name(found);
            if prune.skip_hidden && (name.starts_with(b".") || m.hidden) {
                continue;
            }
            let child_id = index.push(name, id, m.flags, m.size, m.mtime);
            if m.flags == FLAG_DIR && visited.insert(found.object) {
                let mut child = path.clone();
                if !child.last().is_some_and(|&b| crate::os::is_sep(b)) {
                    child.push(crate::os::SEP);
                }
                child.extend_from_slice(name);
                // Excluded folders and trash are listed, not entered.
                if !prune.excludes.contains(&child) && !crate::walk::in_trash(&child) {
                    queue.push_back((found.object, child_id, child));
                }
            }
        }
    }
}

/// Indexes the filesystem on a block device or image file. `root_name` is
/// the path its top folder appears under in the index.
pub fn scan(device: &Path, root_name: &[u8]) -> io::Result<Index> {
    let file = File::open(device)?;
    let mut head = [0u8; 2048];
    crate::os::read_exact_at(&file, &mut head, 0)?;
    let mut index = Index::new();
    let prune = Prune { excludes: &[], skip_hidden: false };
    if is_ntfs(&head) {
        let table = ntfs(&file, &head, &AtomicUsize::new(0))?;
        assemble(&mut index, root_name, NTFS_ROOT_RECORD, &table.names, &table.records, &prune);
    } else if u16le(&head, 1024 + 56) == 0xEF53 {
        let (names, meta) = ext(&file, &head[1024..])?;
        assemble(&mut index, root_name, EXT_ROOT_INODE, &names, &meta, &prune);
    } else {
        return Err(bad(format!("{}: neither an ext2/3/4 nor an NTFS filesystem", device.display())));
    }
    Ok(index)
}

fn is_ntfs(boot: &[u8]) -> bool {
    boot.get(3..11) == Some(b"NTFS    ")
}

// ---- ext2 / ext3 / ext4 ---------------------------------------------------

const EXT_ROOT_INODE: u64 = 2;

struct Ext<'a> {
    file: &'a File,
    block_size: u64,
    /// Directory entries carry a type byte (else the name length is 16-bit).
    filetype: bool,
}

impl Ext<'_> {
    fn block(&self, number: u64) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.block_size as usize];
        crate::os::read_exact_at(&self.file, &mut buf, number.checked_mul(self.block_size).ok_or_else(|| bad("block number overflow"))?)?;
        Ok(buf)
    }

    /// The data blocks of a directory, from its inode's block map: an
    /// extent tree (ext4) or direct and indirect pointers (ext2/3).
    fn dir_blocks(&self, inode: &[u8], out: &mut Vec<u64>) -> io::Result<()> {
        const EXTENTS_FL: u64 = 0x8_0000;
        let map = inode.get(40..100).ok_or_else(|| bad("short inode"))?;
        if u32le(inode, 32) & EXTENTS_FL != 0 {
            return self.extents(map, 0, out);
        }
        for i in 0..12 {
            out.extend(Some(u32le(map, i * 4)).filter(|&b| b != 0));
        }
        for (level, slot) in [(1, 12), (2, 13), (3, 14)] {
            self.indirect(u32le(map, slot * 4), level, out)?;
        }
        Ok(())
    }

    fn indirect(&self, block: u64, level: u32, out: &mut Vec<u64>) -> io::Result<()> {
        if block == 0 || out.len() > 1 << 20 {
            return Ok(());
        }
        for pointer in self.block(block)?.chunks_exact(4).map(|c| u32le(c, 0)).filter(|&b| b != 0) {
            if level == 1 { out.push(pointer) } else { self.indirect(pointer, level - 1, out)? }
        }
        Ok(())
    }

    /// Walks one node of an extent tree: `node` starts with its header.
    fn extents(&self, node: &[u8], depth_guard: u32, out: &mut Vec<u64>) -> io::Result<()> {
        if u16le(node, 0) != 0xF30A || depth_guard > 6 {
            return Ok(()); // not an extent node: treat the directory as empty
        }
        let (entries, depth) = (u16le(node, 2) as usize, u16le(node, 6));
        for entry in node.get(12..).unwrap_or(&[]).chunks_exact(12).take(entries) {
            if depth == 0 {
                // Leaf: first block, length (above 32768 means "allocated
                // but unwritten"), start block high and low parts.
                let len = match u16le(entry, 4) {
                    l if l > 32768 => l - 32768,
                    l => l,
                };
                let start = u16le(entry, 6) << 32 | u32le(entry, 8);
                if out.len() as u64 + len > 1 << 20 {
                    return Ok(()); // an absurdly large directory: stop
                }
                out.extend(start..start + len);
            } else {
                let child = u16le(entry, 8) << 32 | u32le(entry, 4);
                self.extents(&self.block(child)?, depth_guard + 1, out)?;
            }
        }
        Ok(())
    }

    /// Reads the entries of one directory data block into `names`.
    fn dir_entries(&self, dir: u64, data: &[u8], names: &mut Names) {
        let mut at = 0;
        while at + 8 <= data.len() {
            let (object, rec_len) = (u32le(data, at), u16le(data, at + 4) as usize);
            if rec_len < 8 || at + rec_len > data.len() {
                break;
            }
            let name_len = if self.filetype { data[at + 6] as usize } else { u16le(data, at + 6) as usize };
            // Inode 0 marks unused space (and the index nodes of big directories).
            if let Some(name) = data.get(at + 8..at + 8 + name_len).filter(|n| object != 0 && *n != b"." && *n != b"..") {
                names.add(dir, object, name);
            }
            at += rec_len;
        }
    }
}

fn ext(file: &File, sb: &[u8]) -> io::Result<(Names, HashMap<u64, Meta>)> {
    const INCOMPAT_FILETYPE: u64 = 0x2;
    const INCOMPAT_META_BG: u64 = 0x10;
    const INCOMPAT_64BIT: u64 = 0x80;
    const INLINE_DATA_FL: u64 = 0x1000_0000;
    let incompat = u32le(sb, 96);
    if incompat & INCOMPAT_META_BG != 0 {
        return Err(bad("this ext filesystem uses the meta_bg layout, which is not supported"));
    }
    let log_block = u32le(sb, 24);
    if log_block > 6 {
        return Err(bad("unreasonable ext block size"));
    }
    let block_size = 1024u64 << log_block;
    let fs = Ext { file, block_size, filetype: incompat & INCOMPAT_FILETYPE != 0 };
    let inodes_per_group = u32le(sb, 40);
    let blocks_per_group = u32le(sb, 32);
    let inode_size = if u32le(sb, 76) == 0 { 128 } else { u16le(sb, 88) };
    let blocks = u32le(sb, 4) | if incompat & INCOMPAT_64BIT != 0 { u32le(sb, 336) << 32 } else { 0 };
    let first_data_block = u32le(sb, 20);
    if inodes_per_group == 0 || blocks_per_group == 0 || inode_size < 128 || inode_size > block_size {
        return Err(bad("damaged ext superblock"));
    }
    let groups = (blocks - first_data_block).div_ceil(blocks_per_group);
    let desc_size = if incompat & INCOMPAT_64BIT != 0 { u16le(sb, 254).max(32) } else { 32 };
    if groups > 1 << 22 {
        return Err(bad("unreasonable ext group count"));
    }
    // The group descriptor table follows the superblock's block.
    let mut descriptors = vec![0u8; (groups * desc_size) as usize];
    crate::os::read_exact_at(&file, &mut descriptors, (first_data_block + 1) * block_size)?;

    let mut meta: HashMap<u64, Meta> = HashMap::new();
    let mut names = Names::default();
    let mut blocks_of_dir = Vec::new();
    for (group, desc) in descriptors.chunks_exact(desc_size as usize).enumerate() {
        const INODE_UNINIT: u64 = 0x1;
        if u16le(desc, 18) & INODE_UNINIT != 0 {
            continue; // no inode of this group was ever used
        }
        let table = u32le(desc, 8) | if desc_size >= 64 { u32le(desc, 40) << 32 } else { 0 };
        // The tail of the table that was never used need not be read.
        let unused = u16le(desc, 28).min(inodes_per_group);
        let used = (inodes_per_group - unused) as usize;
        let mut inodes = vec![0u8; used * inode_size as usize];
        if crate::os::read_exact_at(&file, &mut inodes, table * block_size).is_err() {
            continue;
        }
        for (i, inode) in inodes.chunks_exact(inode_size as usize).enumerate() {
            let number = group as u64 * inodes_per_group + i as u64 + 1;
            let (mode, links) = (u16le(inode, 0), u16le(inode, 26));
            if mode == 0 || links == 0 {
                continue; // free or deleted
            }
            let kind = mode & 0xF000;
            let flags = match kind {
                0x4000 => FLAG_DIR,
                0xA000 => FLAG_SYMLINK,
                _ => 0,
            };
            let size = u32le(inode, 4) | u32le(inode, 108) << 32;
            meta.insert(number, Meta { flags, hidden: false, size, mtime: u32le(inode, 16) as i64 });
            if kind != 0x4000 {
                continue;
            }
            if u32le(inode, 32) & INLINE_DATA_FL != 0 {
                // A tiny directory stored inside the inode: 4 bytes of
                // parent inode, then ordinary entries.
                fs.dir_entries(number, inode.get(44..100).unwrap_or(&[]), &mut names);
                continue;
            }
            blocks_of_dir.clear();
            if fs.dir_blocks(inode, &mut blocks_of_dir).is_ok() {
                for &b in &blocks_of_dir {
                    if let Ok(data) = fs.block(b) {
                        fs.dir_entries(number, &data, &mut names);
                    }
                }
            }
        }
    }
    names.sort();
    Ok((names, meta))
}

// ---- NTFS -------------------------------------------------------------------

const NTFS_ROOT_RECORD: u64 = 5;
const ATTR_STANDARD_INFORMATION: u64 = 0x10;
const ATTR_ATTRIBUTE_LIST: u64 = 0x20;
const ATTR_FILE_NAME: u64 = 0x30;
const ATTR_DATA: u64 = 0x80;
const ATTR_REPARSE_POINT: u64 = 0xC0;

/// A run of clusters: (first cluster on disk, or None for a hole; count).
type Run = (Option<u64>, u64);

/// Decodes a non-resident attribute's run list.
fn runs(mut data: &[u8]) -> Vec<Run> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&head) = data.first().filter(|&&h| h != 0) {
        let (len_size, off_size) = ((head & 0xF) as usize, (head >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 || data.len() < 1 + len_size + off_size {
            break;
        }
        let mut len = [0u8; 8];
        len[..len_size].copy_from_slice(&data[1..1 + len_size]);
        // The offset is a signed delta from the previous run's start.
        let off_bytes = &data[1 + len_size..1 + len_size + off_size];
        let mut off = [if off_bytes.last().is_some_and(|b| b & 0x80 != 0) { 0xFF } else { 0u8 }; 8];
        off[..off_size].copy_from_slice(off_bytes);
        let start = (off_size > 0).then(|| {
            lcn += i64::from_le_bytes(off);
            lcn as u64
        });
        out.push((start, u64::from_le_bytes(len)));
        data = &data[1 + len_size + off_size..];
        if out.len() > 1 << 20 {
            break;
        }
    }
    out
}

/// Undoes NTFS's torn-write protection on a record: the last two bytes of
/// every 512-byte sector were swapped out for a check value. Returns false
/// if the record is not a valid, completely written FILE record.
fn fixup(record: &mut [u8]) -> bool {
    if record.get(..4) != Some(b"FILE") {
        return false;
    }
    let (offset, count) = (u16le(record, 4) as usize, u16le(record, 6) as usize);
    if count < 2 || offset + count * 2 > record.len() || (count - 1) * 512 > record.len() {
        return false;
    }
    let check = [record[offset], record[offset + 1]];
    for sector in 1..count {
        let end = sector * 512;
        if record[end - 2..end] != check {
            return false;
        }
        let saved = [record[offset + sector * 2], record[offset + sector * 2 + 1]];
        record[end - 2..end].copy_from_slice(&saved);
    }
    true
}

/// Calls `visit(type, attribute bytes)` for each attribute of a record.
fn attributes<'a>(record: &'a [u8], mut visit: impl FnMut(u64, &'a [u8])) {
    let mut at = u16le(record, 0x14) as usize;
    for _ in 0..256 {
        let kind = u32le(record, at);
        let len = u32le(record, at + 4) as usize;
        if kind == 0xFFFF_FFFF || len < 16 || at + len > record.len() {
            break;
        }
        visit(kind, &record[at..at + len]);
        at += len;
    }
}

/// The value of a resident attribute.
fn resident(attr: &[u8]) -> Option<&[u8]> {
    if attr.get(8) != Some(&0) {
        return None;
    }
    let (len, off) = (u32le(attr, 16) as usize, u16le(attr, 20) as usize);
    attr.get(off..off + len)
}

/// An unnamed attribute's run list, if it is stored outside the record.
fn nonresident_runs(attr: &[u8]) -> Option<(u64, Vec<Run>)> {
    if attr.get(8) != Some(&1) || attr[9] != 0 {
        return None; // resident, or a named stream
    }
    Some((u64le(attr, 16), runs(attr.get(u16le(attr, 32) as usize..)?)))
}

/// Marks a record that is free, a system file, or holds no file of its own.
const ABSENT: u8 = 0xFF;
const NO_META: Meta = Meta { flags: ABSENT, hidden: false, size: 0, mtime: 0 };

/// The [`Meta`] of every master file table record, by record number.
struct Records(Vec<Meta>);

impl Objects for Records {
    fn meta(&self, object: u64) -> Option<Meta> {
        self.0.get(object as usize).copied().filter(|m| m.flags != ABSENT)
    }
}

/// What a master file table holds: every name, and every record's details.
struct NtfsTable {
    names: Names,
    records: Records,
}

/// Reads a whole NTFS master file table, front to back in large chunks.
/// `progress` counts the names found so far.
fn ntfs(file: &File, boot: &[u8], progress: &AtomicUsize) -> io::Result<NtfsTable> {
    let sector = u16le(boot, 0x0B);
    // Sizes above 0x80 are a negative power of two, per the NTFS format.
    let pow = |v: u8| if v > 0x80 { 1u64 << (256 - v as u32).min(31) } else { v as u64 };
    let cluster = sector * pow(boot[0x0D]);
    let record_size = match boot[0x40] as i8 {
        n if n < 0 => 1u64 << (-(n as i32)).min(20),
        n => n as u64 * cluster,
    };
    if !(512..=65536).contains(&sector) || cluster == 0 || !(512..=1 << 16).contains(&record_size) {
        return Err(bad("damaged NTFS boot sector"));
    }
    let mft_start = u64le(boot, 0x30) * cluster;
    let read_record = |offset: u64| -> Option<Vec<u8>> {
        let mut record = vec![0u8; record_size as usize];
        crate::os::read_exact_at(file, &mut record, offset).ok()?;
        fixup(&mut record).then_some(record)
    };

    // The master file table describes itself in its first record: find
    // where all of its pieces are, and how much of it is in use.
    let first = read_record(mft_start).ok_or_else(|| bad("cannot read the NTFS master file table"))?;
    let mut pieces: Vec<(u64, Vec<Run>)> = Vec::new();
    let mut list: Option<Vec<u8>> = None;
    let mut table_size = 0;
    attributes(&first, |kind, attr| match kind {
        ATTR_DATA => {
            if attr.get(8) == Some(&1) && attr[9] == 0 && u64le(attr, 16) == 0 {
                table_size = u64le(attr, 48);
            }
            pieces.extend(nonresident_runs(attr));
        }
        ATTR_ATTRIBUTE_LIST => {
            // The list of attributes kept in other records; usually inside
            // the record, otherwise in clusters of its own.
            list = resident(attr).map(<[u8]>::to_vec).or_else(|| {
                let (_, list_runs) = nonresident_runs(attr)?;
                let mut data = Vec::new();
                for (start, len) in list_runs {
                    let mut chunk = vec![0u8; (len * cluster).min(1 << 24) as usize];
                    crate::os::read_exact_at(file, &mut chunk, start? * cluster).ok()?;
                    data.extend(chunk);
                }
                Some(data)
            });
        }
        _ => {}
    });
    // A very fragmented table continues in other records, named by the
    // attribute list; those records lie in the part we already know.
    if let Some(list) = list {
        let known: Vec<Run> = pieces.first().map(|p| p.1.clone()).unwrap_or_default();
        let locate = |record: u64| -> Option<u64> {
            let mut vcn = record * record_size / cluster;
            for &(start, len) in &known {
                if vcn < len {
                    return Some(start? * cluster + vcn * cluster + (record * record_size) % cluster);
                }
                vcn -= len;
            }
            None
        };
        let mut at = 0;
        while at + 26 <= list.len() {
            let (kind, len) = (u32le(&list, at), u16le(&list, at + 4) as usize);
            if len < 26 {
                break;
            }
            let holder = u64le(&list, at + 16) & 0xFFFF_FFFF_FFFF;
            if kind == ATTR_DATA && list[at + 6] == 0 && holder != 0 {
                if let Some(record) = locate(holder).and_then(&read_record) {
                    attributes(&record, |kind, attr| {
                        if kind == ATTR_DATA {
                            pieces.extend(nonresident_runs(attr));
                        }
                    });
                }
            }
            at += len;
        }
    }
    pieces.sort_by_key(|p| p.0);
    pieces.dedup_by_key(|p| p.0);

    // Records past the table's size were never written.
    let total = if table_size > 0 { table_size / record_size } else { u64::MAX };
    let mut records: Vec<Meta> = Vec::with_capacity(total.min(1 << 26) as usize);
    let mut names = Names::default();
    let mut extra_sizes = Vec::new();
    let mut number = 0u64;
    // Big reads: a disk spends its time reading, not seeking.
    let chunk_records = ((8 << 20) / record_size).max(1);
    let mut chunk = Vec::new();
    for (start, len) in pieces.iter().flat_map(|p| p.1.iter().copied()) {
        let run_records = len * cluster / record_size;
        let Some(start) = start else {
            number += run_records;
            continue;
        };
        let mut done = 0;
        while done < run_records && number < total {
            let count = chunk_records.min(run_records - done).min(total - number);
            chunk.resize((count * record_size) as usize, 0);
            let before = names.found.len();
            if crate::os::read_exact_at(file, &mut chunk, start * cluster + done * record_size).is_ok() {
                for (i, record) in chunk.chunks_exact_mut(record_size as usize).enumerate() {
                    ntfs_record(number + i as u64, record, &mut records, &mut names, &mut extra_sizes);
                }
            }
            progress.fetch_add(names.found.len() - before, Ordering::Relaxed);
            done += count;
            number += count;
        }
        if number >= total {
            break;
        }
    }
    // Sizes of files whose content is described in an extension record.
    for (base, size) in extra_sizes {
        if let Some(m) = records.get_mut(base as usize).filter(|m| m.flags == 0) {
            m.size = size;
        }
    }
    names.sort();
    Ok(NtfsTable { names, records: Records(records) })
}

/// Takes the names, size, date and kind out of one master file table record.
fn ntfs_record(number: u64, record: &mut [u8], records: &mut Vec<Meta>, names: &mut Names, extra_sizes: &mut Vec<(u64, u64)>) {
    const IN_USE: u64 = 0x1;
    const DIRECTORY: u64 = 0x2;
    const HIDDEN: u64 = 0x2;
    const IO_REPARSE_TAG_MOUNT_POINT: u64 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u64 = 0xA000_000C;
    let slot = number as usize;
    if records.len() <= slot {
        records.resize(slot + 1, NO_META);
    }
    // Records below 16 are the filesystem's own files, except the root
    // folder (5).
    let system = number < 16 && number != NTFS_ROOT_RECORD;
    if system || !fixup(record) || u16le(record, 0x16) & IN_USE == 0 {
        return;
    }
    let base = u64le(record, 0x20) & 0xFFFF_FFFF_FFFF;
    if base != 0 {
        // An extension record: attributes of record `base` that did not
        // fit in it (a file with many names or a very fragmented content).
        attributes(record, |kind, attr| match kind {
            ATTR_FILE_NAME => file_name(base, attr, names),
            ATTR_DATA if attr.get(8) == Some(&1) && attr[9] == 0 && u64le(attr, 16) == 0 => {
                extra_sizes.push((base, u64le(attr, 48)));
            }
            _ => {}
        });
        return;
    }
    let is_dir = u16le(record, 0x16) & DIRECTORY != 0;
    let (mut size, mut mtime, mut hidden, mut link) = (0u64, 0i64, false, false);
    attributes(record, |kind, attr| match kind {
        ATTR_STANDARD_INFORMATION => {
            if let Some(value) = resident(attr) {
                // Times count 100-nanosecond steps from the year 1601.
                mtime = (u64le(value, 8) / 10_000_000) as i64 - 11_644_473_600;
                hidden = u32le(value, 32) & HIDDEN != 0;
            }
        }
        // The root folder's own name is ".", in itself.
        ATTR_FILE_NAME if number != NTFS_ROOT_RECORD => file_name(number, attr, names),
        // The unnamed data stream is the file's content; a stream stored
        // outside the record gives its size in its first piece.
        ATTR_DATA if attr.get(9) == Some(&0) => match attr.get(8) {
            Some(0) => size = u32le(attr, 16),
            _ if u64le(attr, 16) == 0 => size = u64le(attr, 48),
            _ => {}
        },
        // Symbolic links and junctions, which the walk does not follow
        // either. Other reparse points (cloud files, ...) are ordinary.
        ATTR_REPARSE_POINT => {
            link = resident(attr).is_some_and(|v| matches!(u32le(v, 0), IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT));
        }
        _ => {}
    });
    let flags = if link {
        FLAG_SYMLINK
    } else if is_dir {
        FLAG_DIR
    } else {
        0
    };
    records[slot] = Meta { flags, hidden, size: if flags == 0 { size } else { 0 }, mtime };
}

/// Records one `$FILE_NAME` attribute of record `object`.
fn file_name(object: u64, attr: &[u8], names: &mut Names) {
    const DOS_ONLY: u8 = 2; // the short 8.3 alias of a long name
    let Some(value) = resident(attr).filter(|v| v.len() >= 66) else { return };
    let parent = u64le(value, 0) & 0xFFFF_FFFF_FFFF;
    let Some(utf16) = value.get(66..66 + value[64] as usize * 2) else { return };
    if value[65] == DOS_ONLY || parent == object {
        return;
    }
    // Most names are plain ASCII: no conversion needed.
    if utf16.chunks_exact(2).all(|c| c[1] == 0 && c[0] < 0x80) {
        let mut ascii = [0u8; 255];
        let n = utf16.len() / 2;
        for (i, c) in utf16.chunks_exact(2).enumerate() {
            ascii[i] = c[0];
        }
        names.add(parent, object, &ascii[..n]);
        return;
    }
    let units: Vec<u16> = utf16.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    // On Windows, names are kept exactly as the folder walk keeps them,
    // unpaired surrogates included.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        names.add(parent, object, crate::os::bytes(&std::ffi::OsString::from_wide(&units)));
    }
    #[cfg(not(windows))]
    names.add(parent, object, String::from_utf16_lossy(&units).as_bytes());
}

/// Indexes, straight from each volume's master file table, the roots that
/// lie on NTFS volumes this process may open whole (which takes
/// administrator rights). Far faster than walking folders on a hard disk:
/// the table is read front to back, with no seek per folder. Returns the
/// roots left for the folder walk. `FF_NO_MFT=1` turns this off.
#[cfg(windows)]
pub(crate) fn index_volumes(roots: Vec<PathBuf>, index: &mut Index, prune: &Prune, progress: &AtomicUsize) -> Vec<PathBuf> {
    if std::env::var_os("FF_NO_MFT").is_some_and(|v| !v.is_empty()) {
        return roots;
    }
    struct Volume {
        device: PathBuf,
        file: File,
        disk: Option<u32>,
        /// Roots on this volume, with their folder's record number.
        roots: Vec<(PathBuf, u64)>,
    }
    let mut volumes: Vec<Volume> = Vec::new();
    let mut rest = Vec::new();
    for root in roots {
        let found = crate::win::volume_of(&root)
            .and_then(|(_, device)| Ok((device, crate::win::file_record(&File::open(&root)?)?)));
        let Ok((device, record)) = found else {
            rest.push(root);
            continue;
        };
        if let Some(v) = volumes.iter_mut().find(|v| v.device == device) {
            v.roots.push((root, record));
            continue;
        }
        // Opening a whole volume is refused without administrator rights.
        let opened = File::open(&device).and_then(|file| {
            let mut head = [0u8; 512];
            crate::os::read_exact_at(&file, &mut head, 0)?;
            if is_ntfs(&head) { Ok(file) } else { Err(bad("not NTFS")) }
        });
        match opened {
            Ok(file) => {
                let disk = crate::win::disk_number(&file);
                volumes.push(Volume { device, file, disk, roots: vec![(root, record)] });
            }
            Err(_) => rest.push(root),
        }
    }

    // One reader per physical disk: volumes that share a disk are read one
    // after the other, so its head is not dragged back and forth.
    let mut disks: Vec<(Option<u32>, Vec<usize>)> = Vec::new();
    for (i, v) in volumes.iter().enumerate() {
        match disks.iter_mut().find(|d| d.0.is_some() && d.0 == v.disk) {
            Some(d) => d.1.push(i),
            None => disks.push((v.disk, vec![i])),
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        for (_, group) in &disks {
            let (tx, volumes) = (tx.clone(), &volumes);
            s.spawn(move || {
                for &i in group {
                    let file = &volumes[i].file;
                    let mut head = [0u8; 512];
                    let table = crate::os::read_exact_at(file, &mut head, 0).and_then(|()| ntfs(file, &head, progress));
                    if tx.send((i, table)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Each table goes into the index as soon as it is read, so only a
        // few are held in memory at a time.
        for (i, table) in rx {
            for (root, record) in &volumes[i].roots {
                match &table {
                    Ok(t) if t.records.meta(*record).is_some_and(|m| m.flags == FLAG_DIR) => {
                        assemble(index, crate::os::path_bytes(root), *record, &t.names, &t.records, prune);
                    }
                    _ => rest.push(root.clone()),
                }
            }
        }
    });
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::{Query, SearchOptions};
    use std::fs;
    #[cfg(unix)]
    use std::process::Command;

    #[cfg(unix)]
    fn tool(name: &str) -> Option<String> {
        ["/usr/sbin", "/sbin", "/usr/bin"].iter().map(|d| format!("{d}/{name}")).find(|p| Path::new(p).exists())
    }

    /// All paths in an index, relative to its root, sorted.
    #[cfg(unix)]
    fn paths(index: &Index) -> Vec<String> {
        let mut all: Vec<String> = (1..index.len() as u32)
            .map(|id| index.path(id).to_string_lossy().trim_start_matches("/img/").to_string())
            .collect();
        all.sort();
        all
    }

    #[cfg(unix)]
    fn sample_tree(root: &Path) {
        fs::create_dir_all(root.join("docs/deep/deeper")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("docs/report.pdf"), vec![7u8; 5000]).unwrap();
        fs::write(root.join("docs/deep/deeper/note.txt"), "hi").unwrap();
        fs::write(root.join("Ünïcode ☃.txt"), "x").unwrap();
        std::os::unix::fs::symlink("docs/report.pdf", root.join("link")).unwrap();
        // Enough files to need several directory blocks and an index tree.
        fs::create_dir_all(root.join("many")).unwrap();
        for i in 0..400 {
            fs::write(root.join(format!("many/file-with-a-rather-long-name-{i:04}.dat")), "").unwrap();
        }
    }

    #[cfg(unix)]
    fn ext_roundtrip(fs_type: &str) {
        // Skipped quietly where the tool that makes test images is missing.
        let Some(mkfs) = tool("mkfs.ext4") else { return };
        let dir = std::env::temp_dir().join(format!("ff_rawfs_{fs_type}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let tree = dir.join("tree");
        sample_tree(&tree);
        let image = dir.join("fs.img");
        // -d fills the new filesystem from a folder: no mounting, no root.
        let made = Command::new(mkfs)
            .args(["-q", "-F", "-t", fs_type, "-d"])
            .arg(&tree)
            .arg(&image)
            .arg("32M")
            .output()
            .unwrap();
        assert!(made.status.success(), "mkfs failed: {}", String::from_utf8_lossy(&made.stderr));

        let index = scan(&image, b"/img").unwrap();
        let mut expected: Vec<String> = vec![
            "docs", "docs/deep", "docs/deep/deeper", "docs/deep/deeper/note.txt", "docs/report.pdf", "empty", "link",
            "lost+found", "many", "Ünïcode ☃.txt",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        expected.extend((0..400).map(|i| format!("many/file-with-a-rather-long-name-{i:04}.dat")));
        expected.sort();
        assert_eq!(paths(&index), expected);

        let find = |q: &str| index.search(&Query::parse(q), &SearchOptions::default());
        let report = find("report.pdf")[0];
        assert_eq!((index.size(report), index.is_dir(report)), (5000, false));
        assert!(index.is_dir(find("deeper")[0]));
        assert!(index.is_symlink(find("link")[0]));
        assert!(index.mtime(report) > 1_600_000_000);
        // The scanned index is an ordinary one: it saves and loads.
        index.save(dir.join("x.db")).unwrap();
        assert_eq!(Index::load(dir.join("x.db")).unwrap().len(), index.len());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn reads_ext4() {
        ext_roundtrip("ext4");
    }

    #[test]
    #[cfg(unix)]
    fn reads_ext2_block_maps() {
        ext_roundtrip("ext2");
    }

    #[test]
    #[cfg(unix)]
    fn reads_ntfs() {
        let (Some(mkfs), Some(ntfscp)) = (tool("mkfs.ntfs"), tool("ntfscp")) else { return };
        let dir = std::env::temp_dir().join(format!("ff_rawfs_ntfs_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        fs::File::create(&image).unwrap().set_len(32 << 20).unwrap();
        let made = Command::new(mkfs).args(["-q", "-F", "-f"]).arg(&image).output().unwrap();
        assert!(made.status.success(), "mkfs.ntfs failed: {}", String::from_utf8_lossy(&made.stderr));
        // ntfscp writes into an unmounted image (it cannot make folders).
        let source = dir.join("source.bin");
        fs::write(&source, vec![3u8; 70_000]).unwrap();
        for name in ["big file.bin", "Ünïcode ☃.txt", "a-name-that-needs-more-than-eight-dot-three.document"] {
            let copied = Command::new(&ntfscp).arg("-q").arg(&image).arg(&source).arg(name).output().unwrap();
            assert!(copied.status.success(), "ntfscp failed: {}", String::from_utf8_lossy(&copied.stderr));
        }

        let index = scan(&image, b"/img").unwrap();
        let mut found = paths(&index);
        // A fresh NTFS volume has no visible files of its own.
        found.retain(|p| !p.starts_with('$'));
        assert_eq!(found, ["a-name-that-needs-more-than-eight-dot-three.document", "big file.bin", "Ünïcode ☃.txt"]);
        let big = index.search(&Query::parse("\"big file\""), &SearchOptions::default())[0];
        assert_eq!((index.size(big), index.is_dir(big)), (70_000, false));
        assert!(index.mtime(big) > 1_600_000_000);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_other_data() {
        let dir = std::env::temp_dir().join(format!("ff_rawfs_bad_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("junk.img");
        fs::write(&file, vec![0xABu8; 8192]).unwrap();
        assert!(scan(&file, b"/img").is_err());
        fs::write(&file, b"tiny").unwrap();
        assert!(scan(&file, b"/img").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decodes_run_lists() {
        // 0x21: 1 length byte, 2 offset bytes. 0x18 clusters at 0x5634.
        assert_eq!(runs(&[0x21, 0x18, 0x34, 0x56, 0x00]), [(Some(0x5634), 0x18)]);
        // A second run with a negative delta, then a hole.
        assert_eq!(runs(&[0x11, 0x10, 0x50, 0x11, 0x08, 0xF0, 0x01, 0x04, 0x00]), [(Some(0x50), 0x10), (Some(0x40), 8), (None, 4)]);
        assert!(runs(&[0x00]).is_empty());
        assert!(runs(&[0x99, 0x01]).is_empty()); // sizes larger than 8 bytes: ignored
    }
}
