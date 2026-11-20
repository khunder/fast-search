use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use crate::index::{FLAG_DIR, FLAG_SYMLINK, Index, NO_PARENT};

/// What to index.
pub struct BuildOptions {
    /// Directories to index recursively.
    pub roots: Vec<PathBuf>,
    /// Directories to skip entirely (absolute paths).
    pub excludes: Vec<PathBuf>,
    /// Walker threads; 0 picks a default from the CPU count.
    pub threads: usize,
    /// Leave out files and folders whose name starts with a dot.
    pub skip_hidden: bool,
}

/// How "hidden names are left out" is recorded among an index's excludes,
/// so that refreshing the index keeps the choice.
pub(crate) const HIDDEN_MARK: &[u8] = b"<hidden>";

impl BuildOptions {
    /// Indexes `roots`, skipping the kernel pseudo-filesystems and btrfs
    /// snapshot directory by default.
    pub fn new<P: Into<PathBuf>>(roots: impl IntoIterator<Item = P>) -> Self {
        BuildOptions {
            roots: roots.into_iter().map(Into::into).collect(),
            excludes: crate::os::default_excludes(),
            threads: 0,
            skip_hidden: false,
        }
    }

    /// Options that rebuild `old` with the same roots and excludes
    /// (root `/` with the default excludes if `old` is empty).
    pub fn refresh(old: &Index) -> Self {
        if old.is_empty() {
            return Self::new(crate::os::default_roots());
        }
        // Roots that are not folders came from a device read directly
        // (`ff index --device`); those are not walked.
        let mut opts = Self::new(old.roots().map(|r| old.path(r)).filter(|p| p.is_dir()));
        opts.skip_hidden = old.skips_hidden();
        if opts.roots.is_empty() {
            return Self::new(crate::os::default_roots());
        }
        opts.excludes = old.excludes().collect();
        opts
    }
}

struct Queue {
    /// Directories waiting to be read, with their entry id.
    stack: Vec<(PathBuf, u32)>,
    /// Workers currently reading a directory (they may still add work).
    active: usize,
}

impl Index {
    /// Walks the roots in parallel and builds a fresh index. Symlinks are
    /// recorded but not followed; unreadable directories are skipped.
    pub fn build(opts: &BuildOptions) -> io::Result<Index> {
        Self::build_with_progress(opts, &AtomicUsize::new(0))
    }

    /// As [`Index::build`], counting the entries found so far in
    /// `progress` as the walk goes, for a progress display.
    pub fn build_with_progress(opts: &BuildOptions, progress: &AtomicUsize) -> io::Result<Index> {
        let mut roots = Vec::new();
        for r in &opts.roots {
            let p = crate::os::canonicalize(r).map_err(|e| with_path(e, r))?;
            if !p.is_dir() {
                return Err(with_path(io::ErrorKind::NotADirectory.into(), &p));
            }
            roots.push(p);
        }
        // Drop duplicates and roots nested in another root (ancestors sort first).
        roots.sort();
        let mut kept: Vec<PathBuf> = Vec::new();
        for r in roots {
            if !kept.iter().any(|k| r.starts_with(k)) {
                kept.push(r);
            }
        }

        let mut index = Index::new();
        index.excludes = opts
            .excludes
            .iter()
            .map(|e| crate::os::path_bytes(&crate::os::canonicalize(e).unwrap_or_else(|_| e.clone())).to_vec())
            .collect();

        let mut stack = Vec::new();
        for r in kept {
            let md = fs::metadata(&r).map_err(|e| with_path(e, &r))?;
            let id = index.push(crate::os::path_bytes(&r), NO_PARENT, FLAG_DIR, crate::os::size(&md), crate::os::mtime(&md));
            stack.push((r, id));
        }

        let threads = match opts.threads {
            0 => thread::available_parallelism().map_or(4, |n| n.get()).min(16),
            n => n,
        };
        if opts.skip_hidden {
            index.excludes.push(HIDDEN_MARK.to_vec());
        }
        let excludes = std::mem::take(&mut index.excludes);
        let index = Mutex::new(index);
        let queue = (Mutex::new(Queue { stack, active: 0 }), Condvar::new());
        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| worker(&queue, &index, &excludes, progress));
            }
        });
        let mut index = index.into_inner().unwrap();
        index.excludes = excludes;
        Ok(index)
    }
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn worker(queue: &(Mutex<Queue>, Condvar), index: &Mutex<Index>, excludes: &[Vec<u8>], progress: &AtomicUsize) {
    let (lock, cond) = queue;
    let mut batch: Vec<(OsString, u8, u64, i64)> = Vec::new();
    let mut dirs: Vec<(OsString, u32)> = Vec::new();
    loop {
        let (dir, dir_id) = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(job) = q.stack.pop() {
                    q.active += 1;
                    break job;
                }
                if q.active == 0 {
                    cond.notify_all();
                    return;
                }
                q = cond.wait(q).unwrap();
            }
        };

        if let Ok(rd) = fs::read_dir(&dir) {
            let skip_hidden = excludes.iter().any(|x| x == HIDDEN_MARK);
            for entry in rd.flatten() {
                if skip_hidden && crate::os::bytes(&entry.file_name()).starts_with(b".") {
                    continue;
                }
                let (flags, size, mtime) = match entry.metadata() {
                    Ok(md) if skip_hidden && crate::os::hidden_attribute(&md) => continue,
                    Ok(md) => (type_flags(md.file_type()), crate::os::size(&md), crate::os::mtime(&md)),
                    Err(_) => (entry.file_type().map_or(0, type_flags), 0, 0),
                };
                batch.push((entry.file_name(), flags, size, mtime));
            }
        }

        // One short critical section per directory keeps contention low.
        {
            let mut ix = index.lock().unwrap();
            progress.fetch_add(batch.len(), Ordering::Relaxed);
            for (name, flags, size, mtime) in batch.drain(..) {
                let id = ix.push(crate::os::bytes(&name), dir_id, flags, size, mtime);
                if flags == FLAG_DIR {
                    dirs.push((name, id));
                }
            }
        }

        let subdirs: Vec<(PathBuf, u32)> = dirs
            .drain(..)
            .map(|(name, id)| (dir.join(name), id))
            .filter(|(p, _)| !excludes.iter().any(|x| x == crate::os::path_bytes(p)))
            .filter(|(p, _)| !in_trash(crate::os::path_bytes(p)))
            .collect();

        let mut q = lock.lock().unwrap();
        q.stack.extend(subdirs);
        q.active -= 1;
        drop(q);
        cond.notify_all();
    }
}

/// True if `path` is a desktop trash folder or inside one: the user's
/// `~/.local/share/Trash`, or a drive's `.Trash` / `.Trash-<uid>`. Trashed
/// files are never indexed (the folder itself is listed, not its content).
/// On Windows, a drive's `$Recycle.Bin` too.
pub(crate) fn in_trash(path: &[u8]) -> bool {
    let parts: Vec<&[u8]> = path.split(|&b| crate::os::is_sep(b)).collect();
    parts.iter().enumerate().any(|(i, part)| {
        *part == b".Trash"
            || part.starts_with(b".Trash-")
            || (*part == b"Trash" && i >= 2 && parts[i - 1] == b"share" && parts[i - 2] == b".local")
            || (cfg!(windows) && part.eq_ignore_ascii_case(b"$Recycle.Bin"))
    })
}

pub(crate) fn type_flags(ft: fs::FileType) -> u8 {
    if ft.is_symlink() {
        FLAG_SYMLINK
    } else if ft.is_dir() {
        FLAG_DIR
    } else {
        0
    }
}
