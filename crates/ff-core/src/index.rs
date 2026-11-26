use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// An index file mapped into memory, read-only. Its pages are the
/// system's own copy of the file, shared by every program that maps it.
#[cfg(unix)]
struct Map {
    at: *mut u8,
    len: usize,
}

// SAFETY: the mapping is never written through and lives until dropped.
#[cfg(unix)]
unsafe impl Send for Map {}
#[cfg(unix)]
unsafe impl Sync for Map {}

#[cfg(unix)]
impl Map {
    fn open(path: &Path) -> io::Result<Map> {
        use std::os::fd::AsRawFd;
        let file = fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: a new read-only (1), private (2) mapping of a file we
        // hold open; the kernel picks the address.
        let at = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
        if at as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { at, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `at..at + len` is our mapping, valid until drop.
        unsafe { std::slice::from_raw_parts(self.at, self.len) }
    }
}

#[cfg(unix)]
impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: exactly the range `open` mapped.
        unsafe { munmap(self.at, self.len) };
    }
}

/// On Windows the index is read, not mapped: a mapped file cannot be
/// replaced, and the daemon replaces its index on every update.
#[cfg(not(unix))]
struct Map;

#[cfg(not(unix))]
impl Map {
    fn open(_: &Path) -> io::Result<Map> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn bytes(&self) -> &[u8] {
        &[]
    }
}

/// True if a stored name ends in a path separator (a root like `/` or `C:\`).
fn ends_with_sep(name: &[u8]) -> bool {
    name.last().is_some_and(|&b| crate::os::is_sep(b))
}

/// One column of the index: values of our own, or values read straight
/// from the mapped file (which become our own the first time they are
/// changed).
pub(crate) enum Col<T> {
    Owned(Vec<T>),
    /// Pointer and count inside the index's `Map`, which outlives it.
    Mapped(*const T, usize),
}

// SAFETY: a mapped column is read-only memory kept alive by the index.
unsafe impl<T: Send> Send for Col<T> {}
unsafe impl<T: Sync> Sync for Col<T> {}

impl<T> std::ops::Deref for Col<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Col::Owned(values) => values,
            // SAFETY: checked when the column was made: in bounds of the
            // map, aligned, and the map lives as long as the index.
            Col::Mapped(at, len) => unsafe { std::slice::from_raw_parts(*at, *len) },
        }
    }
}

impl<T: Copy> Col<T> {
    /// The values for changing; a mapped column is copied first.
    pub(crate) fn to_mut(&mut self) -> &mut Vec<T> {
        if let Col::Mapped(..) = self {
            *self = Col::Owned(self.to_vec());
        }
        match self {
            Col::Owned(values) => values,
            Col::Mapped(..) => unreachable!(),
        }
    }
}

impl<T> From<Vec<T>> for Col<T> {
    fn from(values: Vec<T>) -> Self {
        Col::Owned(values)
    }
}

/// Parent id of a root entry.
pub const NO_PARENT: u32 = u32::MAX;
pub const FLAG_DIR: u8 = 1;
pub const FLAG_SYMLINK: u8 = 2;
/// The entry was deleted since the index was built: searches and the
/// sizes tree skip it. The next build leaves it out for good.
pub const FLAG_GONE: u8 = 0x80;

const MAGIC: &[u8; 8] = b"FFIDX\x00\x00\x01";

/// The file database. Entries are identified by a dense `u32` id; a parent
/// always has a smaller id than its children.
pub struct Index {
    /// All names back to back; entry `i` is `names[offs[i]..offs[i + 1]]`.
    pub(crate) names: Col<u8>,
    pub(crate) offs: Col<u32>,
    pub(crate) parent: Col<u32>,
    pub(crate) size: Col<u64>,
    pub(crate) mtime: Col<i64>,
    pub(crate) flags: Col<u8>,
    /// Directories that were skipped when building (absolute paths).
    pub(crate) excludes: Vec<Vec<u8>>,
    /// The file the mapped columns point into, if any: held only to keep
    /// it mapped for as long as they are used.
    #[allow(dead_code)]
    map: Option<Arc<Map>>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    pub fn new() -> Self {
        Index {
            names: Vec::new().into(),
            offs: vec![0].into(),
            parent: Vec::new().into(),
            size: Vec::new().into(),
            mtime: Vec::new().into(),
            flags: Vec::new().into(),
            excludes: Vec::new(),
            map: None,
        }
    }

    /// Number of entries (files and directories).
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// File name of an entry (for a root: its full path).
    pub fn name(&self, id: u32) -> &[u8] {
        let i = id as usize;
        &self.names[self.offs[i] as usize..self.offs[i + 1] as usize]
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        let p = self.parent[id as usize];
        (p != NO_PARENT).then_some(p)
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.flags[id as usize] & FLAG_DIR != 0
    }

    pub fn is_symlink(&self, id: u32) -> bool {
        self.flags[id as usize] & FLAG_SYMLINK != 0
    }

    /// Size in bytes.
    pub fn size(&self, id: u32) -> u64 {
        self.size[id as usize]
    }

    /// Modification time, seconds since the Unix epoch.
    pub fn mtime(&self, id: u32) -> i64 {
        self.mtime[id as usize]
    }

    /// Ids of the indexed roots.
    pub fn roots(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len() as u32).filter(|&i| self.parent[i as usize] == NO_PARENT)
    }

    /// Directories skipped at build time.
    pub fn excludes(&self) -> impl Iterator<Item = PathBuf> + '_ {
        let folders = self.excludes.iter().filter(|e| e.as_slice() != crate::walk::HIDDEN_MARK);
        folders.map(|e| crate::os::path_from_bytes(e))
    }

    /// True if names starting with a dot were left out when building.
    pub fn skips_hidden(&self) -> bool {
        self.excludes.iter().any(|e| e.as_slice() == crate::walk::HIDDEN_MARK)
    }

    /// Bytes of heap used by the index.
    pub fn memory_usage(&self) -> usize {
        self.names.len() + self.offs.len() * 4 + self.len() * (4 + 8 + 8 + 1)
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut buf = Vec::new();
        self.write_path(id, &mut buf);
        crate::os::os_string_from_vec(buf).into()
    }

    /// Writes the full path of `id` into `buf` (replacing its contents).
    /// Reuse the buffer across calls to avoid allocations.
    pub fn write_path(&self, id: u32, buf: &mut Vec<u8>) {
        // Pass 1: total length. A separator goes before every non-root
        // component unless the parent already ends in '/' (the "/" root).
        let mut len = 0;
        let mut cur = id;
        loop {
            len += self.name(cur).len();
            let p = self.parent[cur as usize];
            if p == NO_PARENT {
                break;
            }
            len += !ends_with_sep(self.name(p)) as usize;
            cur = p;
        }
        // Pass 2: fill from the back.
        buf.clear();
        buf.resize(len, 0);
        let mut end = len;
        let mut cur = id;
        loop {
            let name = self.name(cur);
            buf[end - name.len()..end].copy_from_slice(name);
            end -= name.len();
            let p = self.parent[cur as usize];
            if p == NO_PARENT {
                break;
            }
            if !ends_with_sep(self.name(p)) {
                end -= 1;
                buf[end] = crate::os::SEP;
            }
            cur = p;
        }
    }

    pub(crate) fn push(&mut self, name: &[u8], parent: u32, flags: u8, size: u64, mtime: i64) -> u32 {
        let id = self.len();
        let end = self.names.len() + name.len();
        assert!(id < NO_PARENT as usize && end <= u32::MAX as usize, "index full");
        self.names.to_mut().extend_from_slice(name);
        self.offs.to_mut().push(end as u32);
        self.parent.to_mut().push(parent);
        self.size.to_mut().push(size);
        self.mtime.to_mut().push(mtime);
        self.flags.to_mut().push(flags);
        id as u32
    }

    /// True if the entry was removed since the index was built.
    pub fn is_gone(&self, id: u32) -> bool {
        self.flags[id as usize] & FLAG_GONE != 0
    }

    /// Marks `ids`, and everything inside them, as removed: at once, in
    /// one pass over the index, without rebuilding it (searches skip
    /// them; ids stay valid).
    pub fn remove(&mut self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        let flags = self.flags.to_mut();
        for &id in ids {
            if let Some(f) = flags.get_mut(id as usize) {
                *f |= FLAG_GONE;
            }
        }
        // Children follow their parent, so one ascending pass carries the
        // mark down to everything below.
        let first = ids.iter().copied().min().unwrap_or(0) as usize;
        for i in first..flags.len() {
            let parent = self.parent[i];
            if parent != NO_PARENT && flags[parent as usize] & FLAG_GONE != 0 {
                flags[i] |= FLAG_GONE;
            }
        }
    }

    /// Sets an entry's size and modification time.
    pub(crate) fn set_meta(&mut self, id: u32, size: u64, mtime: i64) {
        self.size.to_mut()[id as usize] = size;
        self.mtime.to_mut()[id as usize] = mtime;
    }

    /// Gives back memory reserved for growth; worth it once a large index
    /// is complete.
    pub fn shrink(&mut self) {
        self.names.to_mut().shrink_to_fit();
        self.offs.to_mut().shrink_to_fit();
        self.parent.to_mut().shrink_to_fit();
        self.size.to_mut().shrink_to_fit();
        self.mtime.to_mut().shrink_to_fit();
        self.flags.to_mut().shrink_to_fit();
    }

    /// Makes room for `entries` more entries with `name_bytes` of names.
    fn reserve(&mut self, entries: usize, name_bytes: usize) {
        self.names.to_mut().reserve_exact(name_bytes);
        self.offs.to_mut().reserve_exact(entries);
        self.parent.to_mut().reserve_exact(entries);
        self.size.to_mut().reserve_exact(entries);
        self.mtime.to_mut().reserve_exact(entries);
        self.flags.to_mut().reserve_exact(entries);
    }

    /// Adds all of `other`'s entries (its roots become extra roots here).
    pub fn append(&mut self, other: &Index) {
        let shift = self.len() as u32;
        self.reserve(other.len(), other.names.len());
        for id in 0..other.len() {
            let parent = match other.parent[id] {
                NO_PARENT => NO_PARENT,
                p => p + shift,
            };
            self.push(other.name(id as u32), parent, other.flags[id], other.size[id], other.mtime[id]);
        }
    }

    /// Drops every entry whose `gone` flag is set, renumbering the rest.
    /// A removed entry's children must be flagged too.
    pub(crate) fn compact(&mut self, gone: &[bool]) {
        let mut kept = Index::new();
        kept.excludes = std::mem::take(&mut self.excludes);
        let mut new_id = vec![NO_PARENT; self.len()];
        for id in 0..self.len() {
            if gone[id] {
                continue;
            }
            let parent = match self.parent[id] {
                NO_PARENT => NO_PARENT,
                p => new_id[p as usize],
            };
            new_id[id] = kept.push(self.name(id as u32), parent, self.flags[id], self.size[id], self.mtime[id]);
        }
        *self = kept;
    }

    /// Writes the index to `path` atomically (temp file + rename).
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let res = self.write_to(Path::new(&tmp)).and_then(|()| fs::rename(&tmp, path));
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    fn write_to(&self, path: &Path) -> io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(path)?);
        w.write_all(MAGIC)?;
        for v in [self.len(), self.names.len(), self.excludes.len()] {
            w.write_all(&(v as u32).to_le_bytes())?;
        }
        for v in self.offs.iter() {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in self.parent.iter() {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in self.size.iter() {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in self.mtime.iter() {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(&self.flags)?;
        w.write_all(&self.names)?;
        for e in &self.excludes {
            w.write_all(&(e.len() as u32).to_le_bytes())?;
            w.write_all(e)?;
        }
        w.flush()
    }

    /// Loads an index written by [`Index::save`]. The file is validated, so a
    /// truncated or corrupt file yields an error rather than a bad index.
    ///
    /// The file is mapped rather than read: its contents are used in place,
    /// so loading costs no memory of our own, and programs that load the
    /// same file share one copy. Saving writes a new file and renames it
    /// over the old one, which leaves an index loaded earlier untouched.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Index> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not a valid ff index (rebuild it)");
        match Map::open(path.as_ref()) {
            Ok(map) => {
                let map = Arc::new(map);
                Self::parse(map.bytes(), Some(&map)).ok_or_else(invalid)
            }
            // Something that cannot be mapped (a pipe, an odd filesystem).
            Err(_) => Self::parse(&fs::read(path)?, None).ok_or_else(invalid),
        }
    }

    /// With `map` (whose bytes `data` must be), columns point into it
    /// where the machine can use the file's numbers as they are.
    fn parse(data: &[u8], map: Option<&Arc<Map>>) -> Option<Index> {
        fn col<T: Copy, const N: usize>(bytes: &[u8], mapped: bool, f: fn([u8; N]) -> T) -> Col<T> {
            let at = bytes.as_ptr() as *const T;
            if mapped && cfg!(target_endian = "little") && at.is_aligned() {
                Col::Mapped(at, bytes.len() / N)
            } else {
                Col::Owned(le(bytes, f))
            }
        }
        let mapped = map.is_some();
        let mut r = Reader(data);
        if r.take(8)? != MAGIC {
            return None;
        }
        let n = r.u32()? as usize;
        let names_len = r.u32()? as usize;
        let n_excl = r.u32()? as usize;

        let offs: Col<u32> = col(r.take((n + 1) * 4)?, mapped, u32::from_le_bytes);
        let parent: Col<u32> = col(r.take(n * 4)?, mapped, u32::from_le_bytes);
        let size: Col<u64> = col(r.take(n * 8)?, mapped, u64::from_le_bytes);
        let mtime: Col<i64> = col(r.take(n * 8)?, mapped, i64::from_le_bytes);
        let flags: Col<u8> = col(r.take(n)?, mapped, u8::from_le_bytes);
        let names: Col<u8> = col(r.take(names_len)?, mapped, u8::from_le_bytes);
        let mut excludes = Vec::new();
        for _ in 0..n_excl {
            let len = r.u32()? as usize;
            excludes.push(r.take(len)?.to_vec());
        }

        let offs_ok = offs[0] == 0
            && offs[n] as usize == names_len
            && offs.windows(2).all(|w| w[0] <= w[1]);
        let parents_ok = parent
            .iter()
            .enumerate()
            .all(|(i, &p)| p == NO_PARENT || (p as usize) < i);
        (offs_ok && parents_ok).then(|| Index { names, offs, parent, size, mtime, flags, excludes, map: map.cloned() })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

fn le<T, const N: usize>(bytes: &[u8], f: fn([u8; N]) -> T) -> Vec<T> {
    bytes.chunks_exact(N).map(|c| f(c.try_into().unwrap())).collect()
}
