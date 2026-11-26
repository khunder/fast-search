//! Keeping an index in step with the filesystem.
//!
//! A watcher ([`crate::watch`]) reports *where* something changed as
//! (directory, name) pairs; [`Tracker::apply`] then looks at the disk to
//! learn *what* is true now and patches the index. Not interpreting event
//! types keeps this correct whatever the order or kind of events (create,
//! delete, rename, write).

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::index::{FLAG_DIR, Index, NO_PARENT};
use crate::walk::type_flags;

/// What it takes to patch an index quickly: its directories by path. Paths
/// are kept as 64-bit hashes (a few bytes per directory instead of the
/// path), and a lookup checks the real path, so a collision only means a
/// change is not seen.
pub struct Tracker {
    /// Hash of the full path of every indexed directory -> its entry id.
    dirs: HashMap<u64, u32>,
    /// Names of roots that never change (drives read directly while not
    /// mounted): their directories are left out of `dirs`.
    fixed_roots: Vec<Vec<u8>>,
    /// Removed entries are marked gone in place, keeping every id valid
    /// (for a window showing them), instead of being dropped.
    tombstones: bool,
    /// Entries whose size or time changed since [`Tracker::take_touched`]
    /// (new ones are those past the index's old length).
    touched: Vec<u32>,
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, continued from `h`: hashing a path in pieces gives the same as
/// hashing it whole.
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn ends_with_sep(path: &[u8]) -> bool {
    path.last().is_some_and(|&b| crate::os::is_sep(b))
}

/// One bit per entry of an index of `len` entries, set for `ids`.
fn bits_of(len: usize, ids: impl Iterator<Item = u32>) -> Vec<u64> {
    let mut bits = vec![0u64; len.div_ceil(64)];
    for id in ids {
        if let Some(word) = bits.get_mut(id as usize / 64) {
            *word |= 1 << (id % 64);
        }
    }
    bits
}

/// Whether `id`'s bit is set (never for NO_PARENT and other ids past the end).
fn has_bit(bits: &[u64], id: u32) -> bool {
    bits.get(id as usize / 64).is_some_and(|w| w & (1 << (id % 64)) != 0)
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !ends_with_sep(&path) {
        path.push(crate::os::SEP);
    }
    path.extend_from_slice(name);
    path
}

impl Tracker {
    /// For an index that stays in use while it is patched: removed entries
    /// are marked gone ([`Index::remove`]) and ids never change.
    pub fn new(index: &Index) -> Tracker {
        Self::build(index, Vec::new(), true)
    }

    fn build(index: &Index, fixed_roots: Vec<Vec<u8>>, tombstones: bool) -> Tracker {
        let mut t = Tracker { dirs: HashMap::new(), fixed_roots, tombstones, touched: Vec::new() };
        t.rebuild(index);
        t
    }

    /// Hashes every directory's path, each from its parent's: one pass,
    /// since parents come before their children.
    fn rebuild(&mut self, index: &Index) {
        self.dirs.clear();
        let mut hashes: HashMap<u32, u64> = HashMap::new();
        for id in 0..index.len() {
            // Gone directories have another flag value, and are left out.
            if index.flags[id] != FLAG_DIR {
                continue;
            }
            let parent = index.parent[id];
            let name = index.name(id as u32);
            let h = if parent == NO_PARENT {
                if self.fixed_roots.iter().any(|root| root == name) {
                    continue;
                }
                fnv(FNV_START, name)
            } else {
                // A parent missing here is fixed or gone, and so is this.
                let Some(&h) = hashes.get(&parent) else { continue };
                let h = if ends_with_sep(index.name(parent)) { h } else { fnv(h, &[crate::os::SEP]) };
                fnv(h, name)
            };
            hashes.insert(id as u32, h);
            self.dirs.insert(h, id as u32);
        }
        self.dirs.shrink_to_fit();
    }

    /// The entries whose size or time changed since the last call.
    pub fn take_touched(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.touched)
    }

    /// The indexed directory at `path`, if any.
    fn dir_id(&self, index: &Index, path: &[u8], buf: &mut Vec<u8>) -> Option<u32> {
        let &id = self.dirs.get(&fnv(FNV_START, path))?;
        if index.is_gone(id) {
            return None;
        }
        index.write_path(id, buf);
        (buf[..] == *path).then_some(id)
    }

    /// True if `path` is an excluded directory or inside one.
    fn excluded(index: &Index, path: &[u8]) -> bool {
        // With hidden names left out, so is anything below a hidden folder.
        let hidden = index.skips_hidden() && path.split(|&b| crate::os::is_sep(b)).any(|part| part.starts_with(b"."));
        hidden
            || crate::walk::in_trash(path)
            || index.excludes.iter().any(|x| path.strip_prefix(&x[..]).is_some_and(|rest| rest.is_empty() || crate::os::is_sep(rest[0]) || ends_with_sep(x)))
    }

    /// Re-reads the given (directory path, entry name) pairs from disk and
    /// updates `index` to match. Returns true if the index changed.
    pub fn apply(&mut self, index: &mut Index, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        // Which names to look for under which indexed directory.
        let mut wanted: HashMap<u32, HashSet<&[u8]>> = HashMap::new();
        let skip_hidden = index.skips_hidden();
        let mut buf = Vec::new();
        for (dir, name) in changes {
            if skip_hidden && name.starts_with(b".") {
                continue;
            }
            if let Some(id) = self.dir_id(index, dir, &mut buf).filter(|_| !Self::excluded(index, dir)) {
                wanted.entry(id).or_default().insert(name);
            }
        }
        if wanted.is_empty() {
            return false;
        }
        // One pass over the index finds the entries that already exist:
        // each entry's parent is looked up in a bit per entry, much cheaper
        // than hashing millions of parents.
        let wanted_bits = bits_of(index.len(), wanted.keys().copied());
        let mut existing: HashMap<(u32, &[u8]), u32> = HashMap::new();
        for id in 0..index.len() as u32 {
            let parent = index.parent[id as usize];
            if !has_bit(&wanted_bits, parent) {
                continue;
            }
            if wanted.get(&parent).is_some_and(|names| names.contains(index.name(id))) && !index.is_gone(id) {
                existing.insert((parent, index.name(id)), id);
            }
        }
        // Owned copies, so the index can be modified while we work.
        let work: Vec<(u32, Vec<u8>, Option<u32>)> = wanted
            .iter()
            .flat_map(|(&dir, names)| names.iter().map(move |&n| (dir, n)))
            .map(|(dir, n)| (dir, n.to_vec(), existing.get(&(dir, n)).copied()))
            .collect();

        let mut changed = false;
        let mut dead: Vec<u32> = Vec::new();
        let mut dir_path = Vec::new();
        for (dir, name, old) in work {
            index.write_path(dir, &mut dir_path);
            let path = join(&dir_path, &name);
            let md = fs::symlink_metadata(crate::os::path_from_bytes(&path)).ok();
            let flags = md.as_ref().map(|m| type_flags(m.file_type()));
            match (md, old) {
                // Still there, same kind: refresh size and time in place.
                (Some(md), Some(id)) if flags == Some(index.flags[id as usize]) => {
                    let i = id as usize;
                    if (index.size[i], index.mtime[i]) != (crate::os::size(&md), crate::os::mtime(&md)) {
                        changed = true;
                        if self.tombstones {
                            self.touched.push(id);
                        }
                    }
                    index.set_meta(id, crate::os::size(&md), crate::os::mtime(&md));
                }
                (md, old) => {
                    changed |= md.is_some() || old.is_some();
                    dead.extend(old); // gone, or replaced by another kind
                    if let Some(md) = md {
                        self.add_tree(index, path, dir, &name, &md);
                    }
                }
            }
        }
        if !dead.is_empty() {
            if self.tombstones {
                // Their hashes stay behind, pointing at gone entries, which
                // lookups skip; a folder made again at the same path takes
                // its hash over.
                index.remove(&dead);
            } else {
                // Children always follow their parent, so one ascending
                // pass carries a removal down to everything below it.
                let mut gone = vec![false; index.len()];
                for id in dead {
                    gone[id as usize] = true;
                }
                for i in 0..gone.len() {
                    let parent = index.parent[i] as usize;
                    gone[i] = gone[i] || gone.get(parent).copied().unwrap_or(false);
                }
                index.compact(&gone);
                self.rebuild(index);
            }
        }
        changed
    }

    /// Re-reads whole directories (found changed by [`CatchUp`], with their
    /// modification time now): every name in them now, and every name the
    /// index has for them. Returns true if the index changed.
    pub fn refresh(&mut self, index: &mut Index, dirs: &[(u32, i64)]) -> bool {
        let mut changes = HashSet::new();
        let mut paths: HashMap<u32, Vec<u8>> = HashMap::new();
        for &(id, mtime) in dirs {
            if index.flags[id as usize] != FLAG_DIR {
                continue; // gone since, or not a plain directory
            }
            let mut path = Vec::new();
            index.write_path(id, &mut path);
            // Seen as it is now: not found changed again.
            index.set_meta(id, index.size[id as usize], mtime);
            if let Ok(rd) = fs::read_dir(crate::os::path_from_bytes(&path)) {
                for entry in rd.flatten() {
                    changes.insert((path.clone(), crate::os::bytes(&entry.file_name()).to_vec()));
                }
            }
            paths.insert(id, path);
        }
        if paths.is_empty() {
            return false;
        }
        let dir_bits = bits_of(index.len(), paths.keys().copied());
        for id in 0..index.len() as u32 {
            let parent = index.parent[id as usize];
            if !has_bit(&dir_bits, parent) {
                continue;
            }
            if let Some(path) = paths.get(&parent).filter(|_| !index.is_gone(id)) {
                changes.insert((path.clone(), index.name(id).to_vec()));
            }
        }
        self.apply(index, &changes)
    }

    /// Adds an entry and, for a directory, everything inside it.
    fn add_tree(&mut self, index: &mut Index, path: Vec<u8>, parent: u32, name: &[u8], md: &fs::Metadata) {
        let flags = type_flags(md.file_type());
        let id = index.push(name, parent, flags, crate::os::size(md), crate::os::mtime(md));
        if flags != FLAG_DIR {
            return;
        }
        let mut stack = vec![(path, id)];
        while let Some((dir, dir_id)) = stack.pop() {
            self.dirs.insert(fnv(FNV_START, &dir), dir_id);
            if Self::excluded(index, &dir) {
                continue; // listed, but never descended into
            }
            let Ok(rd) = fs::read_dir(crate::os::path_from_bytes(&dir)) else { continue };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                let name = entry.file_name();
                if index.skips_hidden() && (crate::os::bytes(&name).starts_with(b".") || crate::os::hidden_attribute(&md)) {
                    continue;
                }
                let flags = type_flags(md.file_type());
                let id = index.push(crate::os::bytes(&name), dir_id, flags, crate::os::size(&md), crate::os::mtime(&md));
                if flags == FLAG_DIR {
                    stack.push((join(&dir, crate::os::bytes(&name)), id));
                }
            }
        }
    }
}

/// Catching up with what changed while nothing was watching (the program
/// was not running): every directory whose modification time differs
/// from the index's is re-read. A directory's time changes when entries
/// are added to it, removed or renamed, so this finds those without
/// listing every directory. (A file rewritten in place keeps its folder's
/// time; its new size shows at its next change.)
///
/// The times are read on threads of their own, one per root (each drive
/// at its own pace, in index order, which follows the disk's), so the
/// caller never waits on the disk; the caller builds the paths and
/// applies the results with [`Tracker::refresh`].
pub struct CatchUp {
    next: u32,
    end: u32,
    /// Per root: its thread's queue, the chunk being gathered, and whether
    /// the thread is at work.
    workers: HashMap<u32, Worker>,
    rx: std::sync::mpsc::Receiver<(u32, Vec<(u32, i64)>)>,
    done_tx: std::sync::mpsc::Sender<(u32, Vec<(u32, i64)>)>,
    /// The root each directory seen so far is under.
    root_of: HashMap<u32, u32>,
}

struct Worker {
    tx: std::sync::mpsc::Sender<Vec<(u32, std::path::PathBuf, i64)>>,
    gathered: Vec<(u32, std::path::PathBuf, i64)>,
    busy: bool,
}

/// Directories checked per chunk, and entries looked at per step.
const CHUNK: usize = 20_000;
const SCAN: u32 = 400_000;

impl CatchUp {
    pub fn new(index: &Index) -> CatchUp {
        let (done_tx, rx) = std::sync::mpsc::channel();
        CatchUp { next: 0, end: index.len() as u32, workers: HashMap::new(), rx, done_tx, root_of: HashMap::new() }
    }

    fn worker(&mut self, root: u32) -> &mut Worker {
        let done = self.done_tx.clone();
        self.workers.entry(root).or_insert_with(|| {
            let (tx, jobs) = std::sync::mpsc::channel::<Vec<(u32, std::path::PathBuf, i64)>>();
            std::thread::spawn(move || {
                // Nobody waits for this: it gives way to everything else,
                // and works at most half the time, so even a small laptop
                // hardly notices it.
                crate::os::background_thread();
                for chunk in jobs {
                    let start = std::time::Instant::now();
                    let changed: Vec<(u32, i64)> = chunk
                        .into_iter()
                        .filter_map(|(id, path, old)| crate::os::quick_mtime(&path).filter(|&now| now != old).map(|now| (id, now)))
                        .collect();
                    if done.send((root, changed)).is_err() {
                        return;
                    }
                    std::thread::sleep(start.elapsed());
                }
            });
            Worker { tx, gathered: Vec::new(), busy: false }
        })
    }

    /// Takes in what the threads found (directories changed, with their
    /// time now), and gives free threads their next chunk.
    pub fn step(&mut self, index: &Index) -> Vec<(u32, i64)> {
        let mut found = Vec::new();
        for (root, changed) in self.rx.try_iter() {
            if let Some(w) = self.workers.get_mut(&root) {
                w.busy = false;
            }
            found.extend(changed);
        }
        // Gather more, unless a busy thread already has plenty waiting.
        let mut path = Vec::new();
        let stop = self.next.saturating_add(SCAN).min(self.end);
        while self.next < stop && !self.workers.values().any(|w| w.gathered.len() >= 4 * CHUNK) {
            let id = self.next;
            self.next += 1;
            if index.flags[id as usize] != FLAG_DIR {
                continue;
            }
            let root = match index.parent[id as usize] {
                NO_PARENT => id,
                p => match self.root_of.get(&p) {
                    Some(&r) => r,
                    None => continue, // under a gone or other kind of entry
                },
            };
            self.root_of.insert(id, root);
            index.write_path(id, &mut path);
            let mtime = index.mtime[id as usize];
            self.worker(root).gathered.push((id, crate::os::path_from_bytes(&path).to_path_buf(), mtime));
        }
        if self.next >= self.end {
            self.root_of = HashMap::new();
        }
        for w in self.workers.values_mut() {
            if !w.busy && !w.gathered.is_empty() {
                let take = w.gathered.len().min(CHUNK);
                let chunk: Vec<_> = w.gathered.drain(..take).collect();
                w.busy = w.tx.send(chunk).is_ok();
            }
        }
        found
    }

    /// True once every directory was checked and its result taken.
    pub fn done(&self) -> bool {
        self.next >= self.end && self.workers.values().all(|w| !w.busy && w.gathered.is_empty())
    }
}

/// An index plus its [`Tracker`], for a process that owns the index alone
/// (the daemon): removed entries are dropped and ids renumbered.
pub struct LiveIndex {
    index: Index,
    tracker: Tracker,
}

impl LiveIndex {
    pub fn new(index: Index) -> LiveIndex {
        Self::with_fixed_roots(index, Vec::new())
    }

    /// As `new`, for an index where the roots with these names cannot
    /// change, so nothing needs to be remembered about them.
    pub fn with_fixed_roots(index: Index, fixed_roots: Vec<Vec<u8>>) -> LiveIndex {
        let tracker = Tracker::build(&index, fixed_roots, false);
        LiveIndex { index, tracker }
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn into_index(self) -> Index {
        self.index
    }

    /// Re-reads the given (directory path, entry name) pairs from disk and
    /// updates the index to match. Returns true if the index changed.
    pub fn apply(&mut self, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        self.tracker.apply(&mut self.index, changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, Query, SearchOptions};

    #[test]
    fn follows_creates_deletes_and_writes() {
        let root = std::env::temp_dir().join(format!("ff_live_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("keep/old.txt"), "1").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let mut live = LiveIndex::new(Index::build(&BuildOptions::new([&root])).unwrap());
        let root_bytes = crate::os::path_bytes(&root).to_vec();
        let change = |dir: &[u8], name: &str| HashSet::from([(dir.to_vec(), name.as_bytes().to_vec())]);
        let count = |live: &LiveIndex, q: &str| live.index().search(&Query::parse(q), &SearchOptions::default()).len();

        // A new file, and a new directory arriving with content.
        fs::write(root.join("fresh.txt"), "hello").unwrap();
        fs::create_dir_all(root.join("moved/inner")).unwrap();
        fs::write(root.join("moved/inner/deep.txt"), "x").unwrap();
        let mut batch = change(&root_bytes, "fresh.txt");
        batch.extend(change(&root_bytes, "moved"));
        assert!(live.apply(&batch));
        assert_eq!(count(&live, "fresh.txt"), 1);
        assert_eq!(count(&live, "deep.txt moved/inner/"), 1);

        // A file added inside the directory we just learned about.
        fs::write(root.join("moved/inner/late.txt"), "y").unwrap();
        assert!(live.apply(&change(&join(&join(&root_bytes, b"moved"), b"inner"), "late.txt")));
        assert_eq!(count(&live, "late.txt"), 1);

        // A write changes the size; an unchanged file reports no change.
        fs::write(root.join("fresh.txt"), "hello world").unwrap();
        assert!(live.apply(&change(&root_bytes, "fresh.txt")));
        assert!(!live.apply(&change(&root_bytes, "fresh.txt")));
        let id = live.index().search(&Query::parse("fresh.txt"), &SearchOptions::default())[0];
        assert_eq!(live.index().size(id), 11);

        // Removing a directory removes everything under it.
        let before = live.index().len();
        fs::remove_dir_all(root.join("moved")).unwrap();
        assert!(live.apply(&change(&root_bytes, "moved")));
        assert_eq!(live.index().len(), before - 4);
        assert_eq!(count(&live, "deep.txt"), 0);
        assert_eq!(count(&live, "old.txt keep/"), 1);

        // Unknown directories and vanished names are ignored.
        assert!(!live.apply(&change(b"/nonexistent/dir", "x")));
        assert!(!live.apply(&change(&root_bytes, "never-existed")));

        // The patched index still round-trips through a file.
        let db = root.join("index.db");
        live.index().save(&db).unwrap();
        assert_eq!(Index::load(&db).unwrap().len(), live.index().len());

        // A loaded index is mapped from its file; changing it must still
        // work, and must leave the file as it was.
        let saved = fs::read(&db).unwrap();
        let mut loaded = LiveIndex::new(Index::load(&db).unwrap());
        fs::write(root.join("after-load.txt"), "z").unwrap();
        assert!(loaded.apply(&change(&root_bytes, "after-load.txt")));
        assert_eq!(count(&loaded, "after-load.txt"), 1);
        assert_eq!(count(&loaded, "old.txt keep/"), 1);
        fs::remove_file(root.join("keep/old.txt")).unwrap();
        assert!(loaded.apply(&change(&join(&root_bytes, b"keep"), "old.txt")));
        assert_eq!(count(&loaded, "old.txt"), 0);
        assert_eq!(fs::read(&db).unwrap(), saved);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Changes made while nothing watched are found by folder times.
    #[test]
    fn catches_up_with_changes_made_meanwhile() {
        let root = std::env::temp_dir().join(format!("ff_catch_up_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/quiet")).unwrap();
        fs::write(root.join("a/old.txt"), "1").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let mut index = Index::build(&BuildOptions::new([&root])).unwrap();
        // Folder times count in seconds.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a/new.txt"), "2").unwrap();
        fs::remove_file(root.join("a/old.txt")).unwrap();
        let mut tracker = Tracker::new(&index);
        let mut catch_up = CatchUp::new(&index);
        let mut changed = Vec::new();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !catch_up.done() && std::time::Instant::now() < end {
            changed.extend(catch_up.step(&index));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(catch_up.done());
        // Only the folder that changed is read again.
        assert_eq!(changed.len(), 1);
        assert!(tracker.refresh(&mut index, &changed));
        let find = |index: &Index, q: &str| index.search(&Query::parse(q), &SearchOptions::default()).len();
        assert_eq!((find(&index, "new.txt"), find(&index, "old.txt"), find(&index, "quiet")), (1, 0, 1));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Patching in place (the window's way): ids stay valid, removed
    /// entries are marked gone, and a folder made again is found again.
    #[test]
    fn patches_in_place() {
        let root = std::env::temp_dir().join(format!("ff_tracker_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/f.txt"), "1").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let mut index = Index::build(&BuildOptions::new([&root])).unwrap();
        let mut tracker = Tracker::new(&index);
        let root_bytes = crate::os::path_bytes(&root).to_vec();
        let a = join(&root_bytes, b"a");
        let change = |dir: &[u8], name: &str| HashSet::from([(dir.to_vec(), name.as_bytes().to_vec())]);
        let find = |index: &Index, q: &str| index.search(&Query::parse(q), &SearchOptions::default());
        let f = find(&index, "f.txt")[0];
        let len = index.len();

        fs::remove_dir_all(root.join("a/b")).unwrap();
        assert!(tracker.apply(&mut index, &change(&a, "b")));
        assert_eq!(index.len(), len);
        assert!(index.is_gone(f) && find(&index, "f.txt").is_empty());

        fs::create_dir_all(root.join("a/b")).unwrap();
        assert!(tracker.apply(&mut index, &change(&a, "b")));
        fs::write(root.join("a/b/g.txt"), "2").unwrap();
        assert!(tracker.apply(&mut index, &change(&join(&a, b"b"), "g.txt")));
        assert_eq!(find(&index, "g.txt").len(), 1);
        assert_eq!(find(&index, "b").iter().filter(|&&id| index.is_dir(id)).count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }
}

/// Times the tracker on a real index (FF_DB): `cargo test -p ff-core
/// tracker_timing -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn tracker_timing() {
    let db = std::env::var("FF_DB").unwrap();
    let t = std::time::Instant::now();
    let mut index = Index::load(&db).unwrap();
    eprintln!("load {} entries: {:?}", index.len(), t.elapsed());
    let t = std::time::Instant::now();
    let mut tracker = Tracker::new(&index);
    eprintln!("tracker: {} dirs in {:?}", tracker.dirs.len(), t.elapsed());
    let dir = crate::os::home_dir().unwrap().join("Documents");
    let name = format!("ff_timing_{}.txt", std::process::id());
    fs::write(dir.join(&name), "x").unwrap();
    let change = HashSet::from([(crate::os::path_bytes(&dir).to_vec(), name.as_bytes().to_vec())]);
    let t = std::time::Instant::now();
    let changed = tracker.apply(&mut index, &change);
    eprintln!("apply one new file: {changed} in {:?}", t.elapsed());
    fs::remove_file(dir.join(&name)).unwrap();
    let t = std::time::Instant::now();
    let changed = tracker.apply(&mut index, &change);
    eprintln!("apply its removal: {changed} in {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let mut catch_up = CatchUp::new(&index);
    let (mut stale, mut changed, mut refresh_time) = (0, false, std::time::Duration::ZERO);
    while !catch_up.done() {
        let found = catch_up.step(&index);
        stale += found.len();
        let r = std::time::Instant::now();
        changed |= tracker.refresh(&mut index, &found);
        refresh_time += r.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!("catch-up: {stale} folders changed, index changed: {changed}, {:?} ({refresh_time:?} refreshing)", t.elapsed());
}


/// Times searches on a real index (FF_DB), as typed in the window.
#[cfg(test)]
#[test]
#[ignore]
fn search_timing() {
    use crate::{Query, SearchOptions, Sort};
    let index = Index::load(std::env::var("FF_DB").unwrap()).unwrap();
    for sort in [Sort::None, Sort::Size, Sort::Name, Sort::Path] {
        for q in ["", "t", "te", "tes", "test", "test_log"] {
            let t = std::time::Instant::now();
            let n = index.search(&Query::parse(q), &SearchOptions { sort, ..Default::default() }).len();
            eprintln!("{} {q:>9}: {n:>8} hits in {:?}", sort as u8, t.elapsed());
        }
    }
}
