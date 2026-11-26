//! Following changes under a folder as they happen, for [`crate::live`].
//!
//! A [`Watcher`] reports *where* something changed, as (directory, name)
//! pairs; what changed is then read from the disk. On Windows it uses
//! `ReadDirectoryChangesW` on the whole tree (no administrator rights
//! needed); on Linux, inotify with a watch on every directory, added as
//! directories appear. Root-level whole-filesystem watching (fanotify) is
//! the Linux daemon's business.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

/// What a watcher reports.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Something changed about entry `name` in directory `dir` (full path).
    Changed(Vec<u8>, Vec<u8>),
    /// The system dropped events: only a full rescan is safe.
    Overflow,
    /// The watcher failed and stopped (or, on Linux, could not watch every
    /// directory, the system's limit on watches being reached).
    Lost,
}

/// One thread following one root, until stopped or dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts following everything under `root`. Directories whose full
    /// path starts with one of `excludes` are not followed where that can
    /// be avoided (Linux); their events are filtered later anyway.
    pub fn start(root: PathBuf, excludes: Vec<Vec<u8>>, tx: mpsc::Sender<Event>) -> io::Result<Watcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = imp::start(root, excludes, tx, stop.clone())?;
        Ok(Watcher { stop, thread: Some(thread) })
    }

    /// Stops following and waits for the thread to end.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.halt();
    }
}

#[cfg(windows)]
mod imp {
    use super::Event;
    use std::ffi::{OsString, c_void};
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread::JoinHandle;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: Handle,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn CreateEventW(sa: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> Handle;
        fn ReadDirectoryChangesW(
            dir: Handle,
            buf: *mut c_void,
            len: u32,
            subtree: i32,
            filter: u32,
            returned: *mut u32,
            overlapped: *mut Overlapped,
            routine: *const c_void,
        ) -> i32;
        fn GetOverlappedResultEx(file: Handle, overlapped: *mut Overlapped, n: *mut u32, timeout_ms: u32, alertable: i32) -> i32;
        fn CancelIoEx(file: Handle, overlapped: *mut Overlapped) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    const FILE_LIST_DIRECTORY: u32 = 0x1;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const NOTIFY_FILTER: u32 = 0x1 /* file name */ | 0x2 /* dir name */ | 0x8 /* size */ | 0x10 /* last write */;
    const WAIT_TIMEOUT: i32 = 258;
    const ERROR_IO_INCOMPLETE: i32 = 996;
    const ERROR_OPERATION_ABORTED: i32 = 995;
    const ERROR_NOTIFY_ENUM_DIR: i32 = 1022;
    const INFINITE: u32 = u32::MAX;

    /// A handle that is closed when dropped.
    struct Owned(Handle);

    // SAFETY: kernel handles may be used from any thread.
    unsafe impl Send for Owned {}

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a handle we opened and nobody else closes.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn start(root: PathBuf, _excludes: Vec<Vec<u8>>, tx: mpsc::Sender<Event>, stop: Arc<AtomicBool>) -> io::Result<JoinHandle<()>> {
        let name: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; the result is checked.
        let dir = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if dir == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let dir = Owned(dir);
        // SAFETY: an unnamed manual-reset event.
        let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Owned(event);
        Ok(std::thread::spawn(move || follow(&root, &dir, &event, &stop, &tx)))
    }

    /// Reads change notifications for `root` until `stop` is set.
    fn follow(root: &Path, dir: &Owned, event: &Owned, stop: &AtomicBool, tx: &mpsc::Sender<Event>) {
        // u32 elements: the buffer must be DWORD-aligned. 64 KiB is the most
        // a network drive accepts.
        let mut buf = vec![0u32; 16 * 1024];
        loop {
            let mut ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: event.0 };
            // SAFETY: `buf` and `ov` stay alive until the request completes
            // or is cancelled and waited for below.
            let queued = unsafe {
                ReadDirectoryChangesW(
                    dir.0,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 4) as u32,
                    1,
                    NOTIFY_FILTER,
                    std::ptr::null_mut(),
                    &mut ov,
                    std::ptr::null(),
                )
            };
            if queued == 0 {
                let _ = tx.send(Event::Lost);
                return;
            }
            let mut n = 0u32;
            let result = loop {
                // SAFETY: the request queued above.
                if unsafe { GetOverlappedResultEx(dir.0, &mut ov, &mut n, 500, 0) } != 0 {
                    break Ok(n);
                }
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(WAIT_TIMEOUT | ERROR_IO_INCOMPLETE) if !stop.load(Ordering::Relaxed) => continue,
                    Some(WAIT_TIMEOUT | ERROR_IO_INCOMPLETE) => {
                        // SAFETY: cancel our request, then wait until Windows
                        // no longer uses the buffer.
                        unsafe {
                            CancelIoEx(dir.0, &mut ov);
                            GetOverlappedResultEx(dir.0, &mut ov, &mut n, INFINITE, 0);
                        }
                        return;
                    }
                    _ => break Err(err),
                }
            };
            let events = match result {
                // Nothing returned: more changed than the buffer could hold.
                Ok(0) => vec![Event::Overflow],
                Ok(n) => {
                    // SAFETY: Windows wrote `n` bytes into `buf`.
                    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
                    parse_notifications(bytes).into_iter().filter_map(|rel| changed_entry(root, rel)).collect()
                }
                Err(e) if e.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR) => vec![Event::Overflow],
                Err(e) if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED) => return,
                Err(_) => {
                    let _ = tx.send(Event::Lost);
                    return;
                }
            };
            for e in events {
                if tx.send(e).is_err() {
                    return;
                }
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    /// Decodes the `FILE_NOTIFY_INFORMATION` records of one read: the
    /// changed paths, relative to the watched root. Anything malformed ends
    /// the list.
    pub(super) fn parse_notifications(buf: &[u8]) -> Vec<OsString> {
        let u32at = |i: usize| buf.get(i..i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        let mut out = Vec::new();
        let mut at = 0;
        // Each record: next offset, action, name length in bytes, UTF-16 name.
        while let (Some(next), Some(len)) = (u32at(at), u32at(at + 8)) {
            let Some(name) = buf.get(at + 12..at + 12 + len) else { break };
            let wide: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            out.push(OsString::from_wide(&wide));
            if next == 0 {
                break;
            }
            at += next;
        }
        out
    }

    /// The (directory, name) pair to re-check for a change at `rel` under
    /// `root`.
    pub(super) fn changed_entry(root: &Path, rel: OsString) -> Option<Event> {
        let full = root.join(rel);
        let (dir, name) = (full.parent()?, full.file_name()?);
        Some(Event::Changed(crate::os::path_bytes(dir).to_vec(), crate::os::bytes(name).to_vec()))
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Event;
    use std::collections::HashMap;
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread::JoinHandle;

    unsafe extern "C" {
        fn inotify_init1(flags: c_int) -> c_int;
        fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
        fn poll(fds: *mut PollFd, n: u64, timeout_ms: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    const IN_NONBLOCK: c_int = 0o4000;
    const IN_CLOEXEC: c_int = 0o2000000;
    const IN_ATTRIB: u32 = 0x4;
    const IN_CLOSE_WRITE: u32 = 0x8;
    const IN_MOVED_FROM: u32 = 0x40;
    const IN_MOVED_TO: u32 = 0x80;
    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    const IN_ONLYDIR: u32 = 0x0100_0000;
    const IN_DONT_FOLLOW: u32 = 0x0200_0000;
    const IN_Q_OVERFLOW: u32 = 0x4000;
    const IN_IGNORED: u32 = 0x8000;
    const IN_ISDIR: u32 = 0x4000_0000;
    const MASK: u32 = IN_ATTRIB | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE | IN_ONLYDIR | IN_DONT_FOLLOW;
    const POLLIN: i16 = 1;
    const ENOSPC: i32 = 28;

    struct Fd(c_int);

    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: our descriptor, closed once.
            unsafe { close(self.0) };
        }
    }

    pub fn start(root: PathBuf, excludes: Vec<Vec<u8>>, tx: mpsc::Sender<Event>, stop: Arc<AtomicBool>) -> io::Result<JoinHandle<()>> {
        // SAFETY: plain system call; the result is checked.
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut w = Watches { fd: Fd(fd), dirs: HashMap::new(), excludes, full: false };
        if !w.add_tree(root.as_os_str().as_bytes().to_vec()) {
            return Err(io::Error::last_os_error());
        }
        Ok(std::thread::spawn(move || w.follow(&tx, &stop)))
    }

    struct Watches {
        fd: Fd,
        /// Watch descriptor -> the directory's full path.
        dirs: HashMap<c_int, Vec<u8>>,
        excludes: Vec<Vec<u8>>,
        /// The system's limit on watches was reached.
        full: bool,
    }

    impl Watches {
        fn excluded(&self, path: &[u8]) -> bool {
            self.excludes.iter().any(|x| path.strip_prefix(&x[..]).is_some_and(|rest| rest.is_empty() || rest[0] == b'/'))
        }

        /// Watches `top` and every directory below it (not following
        /// symlinks or crossing into excluded folders). False if `top`
        /// itself could not be watched.
        fn add_tree(&mut self, top: Vec<u8>) -> bool {
            let mut first = true;
            let mut stack = vec![top];
            while let Some(dir) = stack.pop() {
                if self.excluded(&dir) || self.full {
                    continue;
                }
                let Ok(c) = CString::new(dir.clone()) else { continue };
                // SAFETY: a NUL-terminated path.
                let wd = unsafe { inotify_add_watch(self.fd.0, c.as_ptr(), MASK) };
                if wd < 0 {
                    if io::Error::last_os_error().raw_os_error() == Some(ENOSPC) {
                        self.full = true;
                    }
                    if first {
                        return false;
                    }
                    continue;
                }
                first = false;
                let Ok(rd) = std::fs::read_dir(std::ffi::OsStr::from_bytes(&dir)) else {
                    self.dirs.insert(wd, dir);
                    continue;
                };
                for entry in rd.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        let mut child = dir.clone();
                        if child.last() != Some(&b'/') {
                            child.push(b'/');
                        }
                        child.extend_from_slice(entry.file_name().as_bytes());
                        stack.push(child);
                    }
                }
                self.dirs.insert(wd, dir);
            }
            true
        }

        fn follow(mut self, tx: &mpsc::Sender<Event>, stop: &AtomicBool) {
            let mut told_full = false;
            // u64 elements keep the event records aligned.
            let mut buf = vec![0u64; 8 * 1024];
            while !stop.load(Ordering::Relaxed) {
                if self.full && !told_full {
                    told_full = true;
                    let _ = tx.send(Event::Lost);
                }
                let mut p = PollFd { fd: self.fd.0, events: POLLIN, revents: 0 };
                // SAFETY: one valid pollfd.
                if unsafe { poll(&mut p, 1, 500) } <= 0 {
                    continue;
                }
                // SAFETY: reads into our buffer, up to its size.
                let n = unsafe { read(self.fd.0, buf.as_mut_ptr().cast(), buf.len() * 8) };
                if n <= 0 {
                    continue;
                }
                // SAFETY: the kernel wrote `n` bytes.
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize).to_vec() };
                let mut out = Vec::new();
                let mut new_dirs = Vec::new();
                for (wd, mask, name) in parse_events(&bytes) {
                    if mask & IN_Q_OVERFLOW != 0 {
                        out.push(Event::Overflow);
                        continue;
                    }
                    if mask & IN_IGNORED != 0 {
                        self.dirs.remove(&wd);
                        continue;
                    }
                    let (Some(dir), false) = (self.dirs.get(&wd), name.is_empty()) else { continue };
                    if mask & IN_ISDIR != 0 && mask & (IN_CREATE | IN_MOVED_TO) != 0 {
                        let mut child = dir.clone();
                        if child.last() != Some(&b'/') {
                            child.push(b'/');
                        }
                        child.extend_from_slice(name);
                        new_dirs.push(child);
                    }
                    out.push(Event::Changed(dir.clone(), name.to_vec()));
                }
                // A new folder may have been filled before its watch was
                // added: its contents are read when it is added to the
                // index, which happens after this event.
                for dir in new_dirs {
                    self.add_tree(dir);
                }
                for e in out {
                    if tx.send(e).is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// The (watch, mask, name) of each `inotify_event` record.
    pub(super) fn parse_events(buf: &[u8]) -> Vec<(c_int, u32, &[u8])> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 16 <= buf.len() {
            let u32at = |i: usize| u32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
            let (wd, mask, len) = (u32at(at) as c_int, u32at(at + 4), u32at(at + 12) as usize);
            let Some(name) = buf.get(at + 16..at + 16 + len) else { break };
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            out.push((wd, mask, &name[..end]));
            at += 16 + len;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for an event naming `name`.
    fn expect(rx: &mpsc::Receiver<Event>, name: &str) -> (Vec<u8>, Vec<u8>) {
        let end = Instant::now() + Duration::from_secs(5);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(Event::Changed(dir, n)) = rx.recv_timeout(left) {
                if n == name.as_bytes() {
                    return (dir, n);
                }
            }
        }
        panic!("no event for {name}");
    }

    #[test]
    fn reports_changes_in_new_and_old_folders() {
        let root = std::env::temp_dir().join(format!("ff_watch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("old")).unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let (tx, rx) = mpsc::channel();
        let w = Watcher::start(root.clone(), Vec::new(), tx).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(root.join("old").join("a.txt"), "1").unwrap();
        let (dir, _) = expect(&rx, "a.txt");
        assert_eq!(dir, crate::os::path_bytes(&root.join("old")));
        std::fs::create_dir(root.join("new")).unwrap();
        expect(&rx, "new");
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(root.join("new").join("b.txt"), "2").unwrap();
        expect(&rx, "b.txt");
        std::fs::remove_file(root.join("old").join("a.txt")).unwrap();
        expect(&rx, "a.txt");
        w.stop();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn decodes_notifications() {
        use std::ffi::OsString;
        use std::path::Path;
        let record = |next: u32, action: u32, name: &str| {
            let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut r = next.to_le_bytes().to_vec();
            r.extend(action.to_le_bytes());
            r.extend((wide.len() as u32).to_le_bytes());
            r.extend(wide);
            r.resize(r.len().next_multiple_of(4), 0);
            r
        };
        let first = record(0, 1, "dir\\new.txt");
        let mut buf = record(first.len() as u32, 1, "dir\\new.txt");
        buf.extend(record(0, 4, "Ünïcode ☃.txt"));
        assert_eq!(imp::parse_notifications(&buf), [OsString::from("dir\\new.txt"), OsString::from("Ünïcode ☃.txt")]);
        // Truncated input stops cleanly.
        for cut in 0..buf.len() {
            assert!(imp::parse_notifications(&buf[..cut]).len() <= 2);
        }
        let Some(Event::Changed(dir, name)) = imp::changed_entry(Path::new(r"C:\"), OsString::from(r"a\b.txt")) else { panic!() };
        assert_eq!((&dir[..], &name[..]), (&br"C:\a"[..], &b"b.txt"[..]));
        let Some(Event::Changed(dir, _)) = imp::changed_entry(Path::new(r"C:\"), OsString::from("top.txt")) else { panic!() };
        assert_eq!(dir, br"C:\");
    }

    #[cfg(not(windows))]
    #[test]
    fn decodes_inotify_events() {
        let mut buf = Vec::new();
        for (wd, mask, name) in [(1i32, 0x100u32, "new.txt"), (2, 0x8000, "")] {
            let mut padded = name.as_bytes().to_vec();
            if !padded.is_empty() {
                padded.resize((name.len() + 1).next_multiple_of(16), 0);
            }
            buf.extend(wd.to_ne_bytes());
            buf.extend(mask.to_ne_bytes());
            buf.extend(0u32.to_ne_bytes());
            buf.extend((padded.len() as u32).to_ne_bytes());
            buf.extend(padded);
        }
        let events = imp::parse_events(&buf);
        assert_eq!(events, [(1, 0x100, &b"new.txt"[..]), (2, 0x8000, &b""[..])]);
        for cut in 0..buf.len() {
            assert!(imp::parse_events(&buf[..cut]).len() <= 2);
        }
    }
}
