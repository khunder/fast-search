//! The Windows daemon: keeps the ff index current.
//!
//! Scans the indexed drives once, then follows each with
//! `ReadDirectoryChangesW` (recursive, one thread per root) and patches the
//! index as files come and go. The index is published to
//! `C:\ProgramData\ff\index.db`, which every user may read, by the
//! `ffd` service (or by `ffd` running in a console).

use std::collections::HashSet;
use std::ffi::{OsString, c_void};
use std::fs;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ff_core::live::LiveIndex;
use ff_core::{BuildOptions, Index};

const USAGE: &str = "\
ffd - keeps the ff index current

USAGE:
  ffd [OPTIONS] [ROOT...]           index ROOTs (default: every fixed drive)
                                    and follow changes, in this console
  ffd --install-service             install and start ffd as a Windows service
                                    (run as administrator)
  ffd --uninstall-service           stop and remove that service
  ffd --configure [--skip DRIVE]... [--exclude DIR]... [--no-hidden]
                                    save settings and restart the service

OPTIONS:
  -x, --exclude DIR   skip a folder (repeatable)
  -o, --output FILE   where to publish the index
                      (default C:\\ProgramData\\ff\\index.db)
  -i, --interval SEC  how often to publish after changes (default 2)
      --skip DRIVE    do not index this drive (e.g. D:)
  Settings are also read from C:\\ProgramData\\ff\\ffd.conf. Drives being
  added or removed are noticed and the index follows.
  -h, --help

The service runs as LocalSystem, and every user of this computer can
search the names of ALL files on the indexed drives, including those in
folders they cannot open.
";

const SERVICE_NAME: &str = "ffd";
const CONFIG_FILE: &str = r"C:\ProgramData\ff\ffd.conf";

/// Makes `dir` (C:\ProgramData\ff) a real folder owned by Administrators
/// that only SYSTEM and Administrators may change and everyone may read.
///
/// ProgramData lets every user create files and folders in it, and what
/// someone creates there is theirs; the service, running as SYSTEM, must
/// not write where a user chose (a planted junction, a file of theirs).
/// Needs administrator rights: run by `--install-service`, `--configure`
/// and the service itself as it starts.
fn secure_folder(dir: &Path) -> Result<(), String> {
    const REPARSE_POINT: u32 = 0x400;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    if let Ok(md) = fs::symlink_metadata(dir) {
        let attrs = std::os::windows::fs::MetadataExt::file_attributes(&md);
        if md.file_type().is_symlink() || attrs & REPARSE_POINT != 0 {
            // A link put in our place: remove the link itself (never what
            // it points to) and make the real folder.
            log(format!("{} was a link; replacing it with a folder", dir.display()));
            fs::remove_dir(dir).or_else(|_| fs::remove_file(dir)).map_err(|e| fail(&e))?;
        } else if !md.is_dir() {
            return Err(fail(&"exists and is not a folder"));
        }
    }
    fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    let icacls = |args: &[&str]| {
        std::process::Command::new("icacls")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // SIDs, not names, which are translated: SYSTEM, Administrators, Users.
    let ok = icacls(&["/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"])
        && icacls(&[
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
            "/Q",
        ]);
    if !ok {
        return Err(fail(&"could not set who may change this folder (run this as administrator)"));
    }
    // What is already inside takes the folder's rules.
    if fs::read_dir(dir).is_ok_and(|mut rd| rd.next().is_some()) {
        let _ = std::process::Command::new("icacls")
            .arg(dir.join("*"))
            .args(["/reset", "/T", "/C", "/Q"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

/// Where log lines are also written, next to the published index.
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    eprintln!("ffd: {msg}");
    if let Some(path) = LOG_FILE.get() {
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{msg}");
        }
    }
}

enum Msg {
    /// Something changed about entry `name` in directory `dir`.
    Changed(Vec<u8>, Vec<u8>),
    /// Windows dropped events for a root; only a full rescan is safe.
    Overflow,
    /// A root's watcher failed and stopped; it is started again (with a
    /// rescan) after a pause, so a lasting failure does not spin.
    Lost,
}

/// How long to wait before restarting a watcher that failed.
const RETRY_LOST: Duration = Duration::from_secs(30);

struct Config {
    /// Roots named on the command line; none means every fixed drive.
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    output: PathBuf,
    interval: Duration,
    settings: Settings,
}

/// What `--configure` saves in ffd.conf: one `skip D:`, `exclude <folder>`
/// or `nohidden` per line (the same format as /etc/ffd.conf on Linux).
#[derive(Debug, Default, Clone, PartialEq)]
struct Settings {
    /// Drives (`D:`) that are not indexed.
    skip: Vec<String>,
    excludes: Vec<String>,
    no_hidden: bool,
}

impl Settings {
    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(folder) = line.strip_prefix("exclude ").map(str::trim).filter(|f| valid_folder(f)) {
                s.excludes.push(folder.to_string());
            } else if line == "nohidden" {
                s.no_hidden = true;
            } else if let Some(drive) = line.strip_prefix("skip ").and_then(|d| drive_name(d.trim())) {
                s.skip.push(drive);
            }
        }
        s
    }

    fn to_text(&self) -> String {
        let mut text = String::from("# Fast search daemon settings (written by ffd --configure).\r\n");
        if self.no_hidden {
            text += "nohidden\r\n";
        }
        for folder in &self.excludes {
            text += &format!("exclude {folder}\r\n");
        }
        for drive in &self.skip {
            text += &format!("skip {drive}\r\n");
        }
        text
    }
}

/// A folder path we are willing to store: absolute, on one line.
fn valid_folder(path: &str) -> bool {
    Path::new(path).is_absolute() && !path.chars().any(char::is_control)
}

/// `d`, `D:` or `D:\` -> `D:`.
fn drive_name(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let ok = !b.is_empty() && b[0].is_ascii_alphabetic() && (b.len() == 1 || (b[1] == b':' && (b.len() == 2 || (b.len() == 3 && b[2] == b'\\'))));
    ok.then(|| format!("{}:", (b[0] as char).to_ascii_uppercase()))
}

pub fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("ffd: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut cfg = Config {
        roots: Vec::new(),
        excludes: Vec::new(),
        output: ff_core::SYSTEM_DB.into(),
        interval: Duration::from_secs(2),
        settings: Settings::parse(&fs::read_to_string(CONFIG_FILE).unwrap_or_default()),
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return Ok(());
            }
            Some("--install-service") => return install_service(),
            Some("--uninstall-service") => return uninstall_service(),
            Some("--configure") => return configure(args.map(|a| a.to_string_lossy().into_owned()).collect()),
            // How the service manager starts us (see `install_service`).
            Some("--service") => return service::run(),
            Some(f @ "--skip") => {
                let text = value(f)?.to_string_lossy().into_owned();
                cfg.settings.skip.push(drive_name(&text).ok_or(format!("{f}: {text} is not a drive like D:"))?);
            }
            Some(f @ ("-x" | "--exclude")) => cfg.excludes.push(PathBuf::from(value(f)?)),
            Some(f @ ("-o" | "--output")) => cfg.output = value(f)?.into(),
            Some(f @ ("-i" | "--interval")) => {
                let secs = value(f)?.to_str().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s >= 0.1);
                cfg.interval = Duration::from_secs_f64(secs.ok_or("--interval needs a number of seconds >= 0.1")?);
            }
            Some(f) if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option {f} (see ffd --help)")),
            _ => cfg.roots.push(PathBuf::from(arg)),
        }
    }
    serve(&cfg, &AtomicBool::new(false))
}

/// The roots to index now: those named, or every fixed drive that is not
/// skipped. Compared between checks to notice drives coming and going.
fn current_roots(cfg: &Config) -> Vec<PathBuf> {
    let roots = if cfg.roots.is_empty() { ff_core::os::default_roots() } else { cfg.roots.clone() };
    let skipped = |root: &Path| {
        let name = root.to_string_lossy();
        drive_name(name.get(..2).unwrap_or("")).is_some_and(|d| cfg.settings.skip.contains(&d) && name.len() <= 3)
    };
    let mut roots: Vec<PathBuf> =
        roots.iter().filter(|r| !skipped(r)).filter_map(|r| ff_core::os::canonicalize(r).ok()).filter(|r| r.is_dir()).collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Indexes and follows the roots until `stop` is set.
fn serve(cfg: &Config, stop: &AtomicBool) -> Result<(), String> {
    if let Some(dir) = cfg.output.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let log_file = cfg.output.with_file_name("ffd.log");
    let _ = fs::remove_file(&log_file);
    let _ = LOG_FILE.set(log_file);
    // Changes to our own files (the index, its temporary copy, the log)
    // and to their folder are not news.
    let own_dir = cfg.output.parent().and_then(|d| std::path::absolute(d).ok()).map(ff_core::os::strip_verbatim);

    let (tx, rx) = mpsc::channel();
    let scan = |why: &str, roots: &[PathBuf], watchers: &mut Vec<Watcher>| -> Result<LiveIndex, String> {
        let start = Instant::now();
        // Start watching before the scan so nothing that changes during
        // it is missed; those events are applied once the scan is done.
        for old in watchers.drain(..) {
            old.stop();
        }
        for root in roots {
            match Watcher::start(root.clone(), tx.clone()) {
                Ok(w) => {
                    log(format!("watching {}", root.display()));
                    watchers.push(w);
                }
                Err(e) => log(format!("{} is NOT watched; changes there will be missed: {e}", root.display())),
            }
        }
        let mut opts = BuildOptions::new(roots.to_vec());
        opts.excludes.extend(cfg.excludes.iter().cloned());
        opts.excludes.extend(cfg.settings.excludes.iter().map(PathBuf::from));
        opts.skip_hidden = cfg.settings.no_hidden;
        let mut index = if roots.is_empty() { Index::new() } else { Index::build(&opts).map_err(|e| e.to_string())? };
        index.shrink();
        log(format!("{why}: {} entries in {:.2}s", index.len(), start.elapsed().as_secs_f64()));
        Ok(LiveIndex::new(index))
    };

    let mut watchers = Vec::new();
    let mut roots = current_roots(cfg);
    let mut live = scan("indexed", &roots, &mut watchers)?;
    publish(cfg, live.index())?;
    log(format!("publishing to {}", cfg.output.display()));

    let mut last_check = Instant::now();
    // When to start again watchers that failed.
    let mut retry_at: Option<Instant> = None;
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // Wake often enough to notice a stop request promptly.
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("the event readers stopped".into()),
        };
        // A drive was added or removed: start over.
        if last_check.elapsed() >= Duration::from_secs(3) {
            last_check = Instant::now();
            let now = current_roots(cfg);
            let retry = retry_at.is_some_and(|at| Instant::now() >= at);
            if now != roots || retry {
                let why = if now != roots { "drives changed, rescanned" } else { "a watcher had stopped, rescanned" };
                roots = now;
                retry_at = None;
                live = scan(why, &roots, &mut watchers)?;
                if let Err(e) = publish(cfg, live.index()) {
                    log(e);
                }
                continue;
            }
        }
        let Some(first) = first else { continue };
        // Gather a batch of changes.
        sleep_unless(stop, cfg.interval);
        let mut changes = HashSet::new();
        let mut overflow = false;
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                Msg::Changed(dir, name) if own_dir.as_deref().is_some_and(|own| is_own(own, &dir, &name)) => {}
                Msg::Changed(dir, name) => {
                    changes.insert((dir, name));
                }
                Msg::Overflow => overflow = true,
                Msg::Lost => {
                    retry_at.get_or_insert_with(|| Instant::now() + RETRY_LOST);
                }
            }
        }
        let changed = if overflow {
            live = scan("events were dropped, rescanned", &roots, &mut watchers)?;
            true
        } else {
            live.apply(&changes)
        };
        if changed {
            if let Err(e) = publish(cfg, live.index()) {
                log(e);
            }
        }
    };
    for w in watchers {
        w.stop();
    }
    result
}

fn sleep_unless(stop: &AtomicBool, total: Duration) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// True if `dir\name` is the folder `own` or inside it.
fn is_own(own: &Path, dir: &[u8], name: &[u8]) -> bool {
    let full = ff_core::os::path_from_bytes(dir).join(ff_core::os::os_string(name));
    let eq = |a: &Path, b: &Path| a.as_os_str().eq_ignore_ascii_case(b.as_os_str());
    full.ancestors().any(|a| eq(a, own))
}

/// Writes the index where readers look for it. A reader holding the file
/// at that moment can make the replace fail; it is tried a few times.
fn publish(cfg: &Config, index: &Index) -> Result<(), String> {
    let mut tries = 0;
    loop {
        match index.save(&cfg.output) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 5 && e.kind() == io::ErrorKind::PermissionDenied => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50 * tries));
            }
            Err(e) => return Err(format!("{}: {e}", cfg.output.display())),
        }
    }
}

// --- Watching ---------------------------------------------------------------

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

/// One thread following one root.
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

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

impl Watcher {
    fn start(root: PathBuf, tx: mpsc::Sender<Msg>) -> io::Result<Watcher> {
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
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || follow(&root, &dir, &event, &flag, &tx));
        Ok(Watcher { stop, thread })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Reads change notifications for `root` until `stop` is set.
fn follow(root: &Path, dir: &Owned, event: &Owned, stop: &AtomicBool, tx: &mpsc::Sender<Msg>) {
    // u32 elements: the buffer must be DWORD-aligned. 64 KiB is the most a
    // network drive accepts.
    let mut buf = vec![0u32; 16 * 1024];
    loop {
        let mut ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: event.0 };
        // SAFETY: `buf` and `ov` stay alive until the request completes or
        // is cancelled and waited for below.
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
            log(format!("{}: watching stopped: {}", root.display(), io::Error::last_os_error()));
            let _ = tx.send(Msg::Lost);
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
                    // SAFETY: cancel our request, then wait until Windows no
                    // longer uses the buffer.
                    unsafe {
                        CancelIoEx(dir.0, &mut ov);
                        GetOverlappedResultEx(dir.0, &mut ov, &mut n, INFINITE, 0);
                    }
                    return;
                }
                _ => break Err(err),
            }
        };
        let msgs = match result {
            // Nothing returned: more changed than the buffer could hold.
            Ok(0) => vec![Msg::Overflow],
            Ok(n) => {
                // SAFETY: Windows wrote `n` bytes into `buf`.
                let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
                parse_notifications(bytes).into_iter().filter_map(|rel| changed_entry(root, rel)).collect()
            }
            Err(e) if e.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR) => vec![Msg::Overflow],
            Err(e) if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED) => return,
            Err(e) => {
                log(format!("{}: watching stopped: {e}", root.display()));
                let _ = tx.send(Msg::Lost);
                return;
            }
        };
        for msg in msgs {
            if tx.send(msg).is_err() {
                return;
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Decodes the `FILE_NOTIFY_INFORMATION` records of one read: the changed
/// paths, relative to the watched root. Anything malformed ends the list.
fn parse_notifications(buf: &[u8]) -> Vec<OsString> {
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

/// The (directory, name) pair to re-check for a change at `rel` under `root`.
fn changed_entry(root: &Path, rel: OsString) -> Option<Msg> {
    let full = root.join(rel);
    let (dir, name) = (full.parent()?, full.file_name()?);
    Some(Msg::Changed(ff_core::os::path_bytes(dir).to_vec(), ff_core::os::bytes(name).to_vec()))
}

// --- Settings and the service -------------------------------------------------

/// `--configure`: saves the settings and restarts a running service so
/// they take effect.
fn configure(args: Vec<String>) -> Result<(), String> {
    let mut s = Settings::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--no-hidden" => s.no_hidden = true,
            "--exclude" => s.excludes.push(args.next().filter(|f| valid_folder(f)).ok_or("--exclude needs an absolute folder path")?),
            "--skip" => s.skip.push(args.next().and_then(|d| drive_name(&d)).ok_or("--skip needs a drive like D:")?),
            _ => return Err(format!("--configure: unknown setting {flag}")),
        }
    }
    let file = Path::new(CONFIG_FILE);
    if let Some(dir) = file.parent() {
        secure_folder(dir)?;
    }
    fs::write(file, s.to_text()).map_err(|e| format!("{CONFIG_FILE}: {e} (run this as administrator)"))?;
    println!("saved {CONFIG_FILE}");
    // Only a running service is restarted; one not installed is ignored.
    if let Ok(scm) = service::Manager::open(false) {
        if let Ok(svc) = scm.service(SERVICE_NAME) {
            if svc.is_running() {
                svc.stop()?;
                svc.start()?;
                println!("restarted the ffd service");
            }
        }
    }
    Ok(())
}

/// `--install-service`: registers this program as the automatically
/// started `ffd` service and (re)starts it.
fn install_service() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = ff_core::os::strip_verbatim(exe);
    let command = format!("\"{}\" --service", exe.display());
    let scm = service::Manager::open(true)?;
    secure_folder(Path::new(ff_core::SYSTEM_DB).parent().unwrap())?;
    let svc = scm.install(SERVICE_NAME, "Fast search index (ffd)", &command)?;
    svc.describe("Keeps the ff file-name index in C:\\ProgramData\\ff current.");
    println!("installed the ffd service: {command}");
    if svc.is_running() {
        svc.stop()?;
    }
    svc.start()?;
    println!("the ffd service is running");
    Ok(())
}

/// `--uninstall-service`: stops and removes the service and its index.
fn uninstall_service() -> Result<(), String> {
    let scm = service::Manager::open(true)?;
    match scm.service(SERVICE_NAME) {
        Ok(svc) => {
            if svc.is_running() {
                svc.stop()?;
            }
            svc.delete()?;
        }
        Err(e) => println!("{e}"),
    }
    let db = Path::new(ff_core::SYSTEM_DB);
    for file in [db.to_path_buf(), db.with_file_name("ffd.log")] {
        match fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(format!("{}: {e}", file.display())),
            _ => {}
        }
    }
    println!("the ffd service is removed");
    Ok(())
}

mod service {
    //! The service control manager: installing ffd, and running under it.

    use super::*;

    type ScHandle = *mut c_void;

    #[repr(C)]
    #[derive(Default)]
    struct ServiceStatus {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_exit_code: u32,
        check_point: u32,
        wait_hint: u32,
    }

    #[repr(C)]
    struct TableEntry {
        name: *const u16,
        proc_: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
    }

    #[repr(C)]
    struct Description {
        text: *const u16,
    }

    type Handler = unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenSCManagerW(machine: *const u16, db: *const u16, access: u32) -> ScHandle;
        fn CreateServiceW(
            scm: ScHandle,
            name: *const u16,
            display: *const u16,
            access: u32,
            kind: u32,
            start: u32,
            error: u32,
            binary: *const u16,
            group: *const u16,
            tag: *mut u32,
            deps: *const u16,
            account: *const u16,
            password: *const u16,
        ) -> ScHandle;
        fn OpenServiceW(scm: ScHandle, name: *const u16, access: u32) -> ScHandle;
        fn ChangeServiceConfigW(
            svc: ScHandle,
            kind: u32,
            start: u32,
            error: u32,
            binary: *const u16,
            group: *const u16,
            tag: *mut u32,
            deps: *const u16,
            account: *const u16,
            password: *const u16,
            display: *const u16,
        ) -> i32;
        fn ChangeServiceConfig2W(svc: ScHandle, level: u32, info: *mut c_void) -> i32;
        fn StartServiceW(svc: ScHandle, argc: u32, argv: *const *const u16) -> i32;
        fn ControlService(svc: ScHandle, control: u32, status: *mut ServiceStatus) -> i32;
        fn QueryServiceStatus(svc: ScHandle, status: *mut ServiceStatus) -> i32;
        fn DeleteService(svc: ScHandle) -> i32;
        fn CloseServiceHandle(h: ScHandle) -> i32;
        fn StartServiceCtrlDispatcherW(table: *const TableEntry) -> i32;
        fn RegisterServiceCtrlHandlerExW(name: *const u16, handler: Handler, context: *mut c_void) -> *mut c_void;
        fn SetServiceStatus(handle: *mut c_void, status: *mut ServiceStatus) -> i32;
    }

    const SC_MANAGER_CONNECT: u32 = 0x1;
    const SC_MANAGER_CREATE_SERVICE: u32 = 0x2;
    const SERVICE_ALL_ACCESS: u32 = 0xF01FF;
    const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
    const SERVICE_AUTO_START: u32 = 2;
    const SERVICE_ERROR_NORMAL: u32 = 1;
    const SERVICE_NO_CHANGE: u32 = u32::MAX;
    const SERVICE_CONFIG_DESCRIPTION: u32 = 1;
    const SERVICE_CONTROL_STOP: u32 = 1;
    const SERVICE_CONTROL_INTERROGATE: u32 = 4;
    const SERVICE_CONTROL_SHUTDOWN: u32 = 5;
    const SERVICE_ACCEPT_STOP: u32 = 1;
    const SERVICE_ACCEPT_SHUTDOWN: u32 = 4;
    const SERVICE_STOPPED: u32 = 1;
    const SERVICE_START_PENDING: u32 = 2;
    const SERVICE_STOP_PENDING: u32 = 3;
    const SERVICE_RUNNING: u32 = 4;
    const ERROR_SERVICE_EXISTS: i32 = 1073;
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn failed(what: &str) -> String {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(ERROR_ACCESS_DENIED) => format!("{what}: access denied (run this as administrator)"),
            Some(ERROR_SERVICE_DOES_NOT_EXIST) => "the ffd service is not installed".into(),
            _ => format!("{what}: {e}"),
        }
    }

    pub struct Manager(ScHandle);

    pub struct Service(ScHandle);

    impl Drop for Manager {
        fn drop(&mut self) {
            // SAFETY: our own handle.
            unsafe { CloseServiceHandle(self.0) };
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            // SAFETY: our own handle.
            unsafe { CloseServiceHandle(self.0) };
        }
    }

    impl Manager {
        /// Connects to the service manager; `create` asks for the right to
        /// add services (administrators only).
        pub fn open(create: bool) -> Result<Manager, String> {
            let access = SC_MANAGER_CONNECT | if create { SC_MANAGER_CREATE_SERVICE } else { 0 };
            // SAFETY: null means this computer and its active database.
            let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
            if h.is_null() { Err(failed("opening the service manager")) } else { Ok(Manager(h)) }
        }

        pub fn service(&self, name: &str) -> Result<Service, String> {
            let name = wide(name);
            // SAFETY: a NUL-terminated name.
            let h = unsafe { OpenServiceW(self.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
            if h.is_null() { Err(failed("opening the ffd service")) } else { Ok(Service(h)) }
        }

        /// Creates the service, or points an existing one at `command`.
        pub fn install(&self, name: &str, display: &str, command: &str) -> Result<Service, String> {
            let (name_w, display_w, command_w) = (wide(name), wide(display), wide(command));
            let null = std::ptr::null();
            // SAFETY: NUL-terminated strings; null account is LocalSystem.
            let h = unsafe {
                CreateServiceW(
                    self.0,
                    name_w.as_ptr(),
                    display_w.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command_w.as_ptr(),
                    null,
                    std::ptr::null_mut(),
                    null,
                    null,
                    null,
                )
            };
            if !h.is_null() {
                return Ok(Service(h));
            }
            if io::Error::last_os_error().raw_os_error() != Some(ERROR_SERVICE_EXISTS) {
                return Err(failed("creating the ffd service"));
            }
            let svc = self.service(name)?;
            // SAFETY: as above; SERVICE_NO_CHANGE keeps the other settings.
            let ok = unsafe {
                ChangeServiceConfigW(
                    svc.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_NO_CHANGE,
                    command_w.as_ptr(),
                    null,
                    std::ptr::null_mut(),
                    null,
                    null,
                    null,
                    display_w.as_ptr(),
                )
            };
            if ok == 0 { Err(failed("updating the ffd service")) } else { Ok(svc) }
        }
    }

    impl Service {
        pub fn describe(&self, text: &str) {
            let text = wide(text);
            let mut d = Description { text: text.as_ptr() };
            // SAFETY: the description lives through the call.
            unsafe { ChangeServiceConfig2W(self.0, SERVICE_CONFIG_DESCRIPTION, (&raw mut d).cast()) };
        }

        fn state(&self) -> u32 {
            let mut s = ServiceStatus::default();
            // SAFETY: plain out-parameter.
            if unsafe { QueryServiceStatus(self.0, &mut s) } == 0 { SERVICE_STOPPED } else { s.current_state }
        }

        pub fn is_running(&self) -> bool {
            self.state() != SERVICE_STOPPED
        }

        /// Waits up to 30 s for the service to reach `state`.
        fn wait_for(&self, state: u32) -> bool {
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                if self.state() == state {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            false
        }

        pub fn start(&self) -> Result<(), String> {
            // SAFETY: no arguments.
            if unsafe { StartServiceW(self.0, 0, std::ptr::null()) } == 0 {
                return Err(failed("starting the ffd service"));
            }
            if self.wait_for(SERVICE_RUNNING) { Ok(()) } else { Err("the ffd service did not start (see C:\\ProgramData\\ff\\ffd.log)".into()) }
        }

        pub fn stop(&self) -> Result<(), String> {
            let mut s = ServiceStatus::default();
            // SAFETY: plain out-parameter.
            if unsafe { ControlService(self.0, SERVICE_CONTROL_STOP, &mut s) } == 0 && self.state() != SERVICE_STOPPED {
                return Err(failed("stopping the ffd service"));
            }
            if self.wait_for(SERVICE_STOPPED) { Ok(()) } else { Err("the ffd service did not stop".into()) }
        }

        pub fn delete(&self) -> Result<(), String> {
            // SAFETY: our own handle.
            if unsafe { DeleteService(self.0) } == 0 { Err(failed("removing the ffd service")) } else { Ok(()) }
        }
    }

    // --- Running as the service ---

    static STOP: AtomicBool = AtomicBool::new(false);
    static STATUS: AtomicUsize = AtomicUsize::new(0);

    fn report(state: u32, exit_code: u32) {
        let accepted = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
        let mut s = ServiceStatus {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            win32_exit_code: exit_code,
            wait_hint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED { 0 } else { 30_000 },
            ..Default::default()
        };
        let handle = STATUS.load(Ordering::SeqCst) as *mut c_void;
        if !handle.is_null() {
            // SAFETY: the handle the service manager gave us.
            unsafe { SetServiceStatus(handle, &mut s) };
        }
    }

    unsafe extern "system" fn handler(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                STOP.store(true, Ordering::SeqCst);
                report(SERVICE_STOP_PENDING, 0);
                0
            }
            SERVICE_CONTROL_INTERROGATE => 0,
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
        let name = wide(SERVICE_NAME);
        // SAFETY: a NUL-terminated name and a handler that lives forever.
        let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), handler, std::ptr::null_mut()) };
        if h.is_null() {
            return;
        }
        STATUS.store(h as usize, Ordering::SeqCst);
        report(SERVICE_START_PENDING, 0);
        // Settings and the index are only trusted from a folder that
        // ordinary users cannot write to.
        if let Err(e) = secure_folder(Path::new(ff_core::SYSTEM_DB).parent().unwrap()) {
            eprintln!("ffd: {e}");
            report(SERVICE_STOPPED, 1);
            return;
        }
        let cfg = Config {
            roots: Vec::new(),
            excludes: Vec::new(),
            output: ff_core::SYSTEM_DB.into(),
            interval: Duration::from_secs(2),
            settings: Settings::parse(&fs::read_to_string(CONFIG_FILE).unwrap_or_default()),
        };
        // Running is reported at once: the first scan of a large drive
        // takes longer than the service manager likes to wait.
        report(SERVICE_RUNNING, 0);
        let result = serve(&cfg, &STOP);
        if let Err(e) = &result {
            log(e);
        }
        report(SERVICE_STOPPED, if result.is_ok() { 0 } else { 1 });
    }

    /// `ffd --service`: hands this process to the service manager, which
    /// calls `service_main` on another thread and returns when it stops.
    pub fn run() -> Result<(), String> {
        let name = wide(SERVICE_NAME);
        let table = [TableEntry { name: name.as_ptr(), proc_: Some(service_main) }, TableEntry { name: std::ptr::null(), proc_: None }];
        // SAFETY: a table ending in a null entry, alive while this blocks.
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            return Err(format!(
                "--service is how Windows starts the installed service; to run in this console, leave it out ({})",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let text = "# comment\nskip d:\nskip D:\\\nskip nonsense\nexclude C:\\Users\\me\\My Videos\nexclude relative\\path\nnohidden\nfuture thing\n";
        let s = Settings::parse(text);
        assert_eq!(s.skip, ["D:", "D:"]);
        assert_eq!(s.excludes, [r"C:\Users\me\My Videos"]);
        assert!(s.no_hidden);
        assert_eq!(Settings::parse(&s.to_text()), s);
        assert_eq!(drive_name("e"), Some("E:".into()));
        assert_eq!(drive_name("E:\\x"), None);
        assert_eq!(drive_name(""), None);
    }

    #[test]
    fn decodes_notifications() {
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
        assert_eq!(parse_notifications(&buf), [OsString::from("dir\\new.txt"), OsString::from("Ünïcode ☃.txt")]);
        // Truncated input stops cleanly.
        for cut in 0..buf.len() {
            assert!(parse_notifications(&buf[..cut]).len() <= 2);
        }
        let Some(Msg::Changed(dir, name)) = changed_entry(Path::new(r"C:\"), OsString::from(r"a\b.txt")) else { panic!() };
        assert_eq!((&dir[..], &name[..]), (&br"C:\a"[..], &b"b.txt"[..]));
        let Some(Msg::Changed(dir, _)) = changed_entry(Path::new(r"C:\"), OsString::from("top.txt")) else { panic!() };
        assert_eq!(dir, br"C:\");
        assert!(is_own(Path::new(r"C:\ProgramData\ff"), br"C:\ProgramData", b"FF"));
        assert!(is_own(Path::new(r"C:\ProgramData\ff"), br"C:\ProgramData\ff", b"index.db.tmp1"));
        assert!(!is_own(Path::new(r"C:\ProgramData\ff"), br"C:\ProgramData", b"other"));
    }

    /// Runs the daemon on a temporary folder and checks that the published
    /// index follows creates, renames and deletes.
    #[test]
    fn follows_a_folder() {
        let base = std::env::temp_dir().join(format!("ffd_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("keep\\old.txt"), "1").unwrap();
        let cfg = Config {
            roots: vec![root.clone()],
            excludes: Vec::new(),
            output: base.join("out\\index.db"),
            interval: Duration::from_millis(100),
            settings: Settings::default(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let db = cfg.output.clone();
        let flag = stop.clone();
        let daemon = std::thread::spawn(move || serve(&cfg, &flag));

        let count = |q: &str| {
            Index::load(&db).map_or(usize::MAX, |ix| ix.search(&ff_core::Query::parse(q), &ff_core::SearchOptions::default()).len())
        };
        let wait = |q: &str, want: usize| {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end {
                if count(q) == want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        };
        assert!(wait("old.txt", 1), "first index");
        // Give the watcher a moment: changes made before it is reading
        // are still caught, as the scan ran after it started.
        fs::write(root.join("fresh.txt"), "hello").unwrap();
        assert!(wait("fresh.txt", 1), "create");
        fs::create_dir_all(root.join("new dir\\inner")).unwrap();
        fs::write(root.join("new dir\\inner\\deep.txt"), "x").unwrap();
        assert!(wait("deep.txt", 1), "new folder with content");
        fs::rename(root.join("fresh.txt"), root.join("keep\\renamed.txt")).unwrap();
        assert!(wait("renamed.txt keep/", 1) && wait("fresh.txt", 0), "rename");
        fs::remove_dir_all(root.join("new dir")).unwrap();
        assert!(wait("deep.txt", 0) && wait("inner", 0), "delete");

        stop.store(true, Ordering::SeqCst);
        daemon.join().unwrap().unwrap();
        fs::remove_dir_all(&base).unwrap();
    }
}
