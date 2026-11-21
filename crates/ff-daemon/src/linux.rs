//! The Linux daemon: keeps the ff index current.
//!
//! Runs as root, scans the disk once, then listens to fanotify events for
//! whole filesystems and patches the index as files come and go. The index
//! is published to a file on a RAM-backed filesystem that the unprivileged
//! `ff` and `ff-gui` read; they never talk to this process.

#[path = "sys.rs"]
mod sys;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use ff_core::live::LiveIndex;
use ff_core::{BuildOptions, Index};

const USAGE: &str = "\
ffd - keeps the ff index current (run as root)

USAGE:
  ffd [OPTIONS] [ROOT...]             index ROOTs (default /) and follow changes
  ffd --install-service GROUP         install and start ffd as a systemd service
  ffd --uninstall-service             stop and remove that service
  ffd --configure [--skip DEV]... [--nolive DEV]... [--device DEV]...
                  [--manual [--include DEV]...] [--exclude DIR]... [--no-hidden]
                                      save per-drive settings and restart
                                      (--manual: index only included drives)

OPTIONS:
  -g, --group NAME    group allowed to read the index (default: root only)
  -x, --exclude DIR   skip a directory (repeatable)
  -o, --output FILE   where to publish the index (default /run/ff/index.db)
  -i, --interval SEC  how often to publish after changes (default 2)
      --skip DEV      do not index this mounted drive (e.g. /dev/sdb1)
      --nolive DEV    index this drive but do not follow its changes
      --device DEV    index this ext2/3/4 or NTFS drive directly while it
                      is not mounted
  Drive settings are also read from /etc/ffd.conf. Drives being mounted,
  unmounted or plugged in are noticed and the index follows.
  -h, --help

Everyone in GROUP can search the names of ALL files on the system,
including those in folders they cannot open.
";

/// Filesystems worth watching; pseudo and network filesystems are skipped.
const WATCHABLE: &[&str] =
    &["btrfs", "ext2", "ext3", "ext4", "xfs", "f2fs", "bcachefs", "zfs", "vfat", "exfat", "ntfs3", "tmpfs", "overlay"];

/// Where log lines are also written, next to the published index, so the
/// reader group can see what the daemon is doing without root.
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
    /// The kernel dropped events; only a full rescan is safe.
    Overflow,
}

struct Config {
    opts: BuildOptions,
    output: PathBuf,
    group: Option<u32>,
    interval: Duration,
    drives: Drives,
}

/// Per-drive settings, by device path such as `/dev/sdb1`. Kept in
/// /etc/ffd.conf, one `skip|nolive|device <path>` per line.
#[derive(Debug, Default, Clone, PartialEq)]
struct Drives {
    /// Mounted drives that are not indexed at all.
    skip: Vec<String>,
    /// Mounted drives that are indexed but not watched for changes.
    nolive: Vec<String>,
    /// Drives to index by reading them directly while they are not mounted.
    devices: Vec<String>,
    /// Newly mounted drives are not indexed by themselves: only those in
    /// `include` are.
    manual: bool,
    include: Vec<String>,
    /// Folders left out of the index, and whether hidden names are.
    excludes: Vec<String>,
    no_hidden: bool,
}

const CONFIG_FILE: &str = "/etc/ffd.conf";

impl Drives {
    fn parse(text: &str) -> Drives {
        let mut drives = Drives::default();
        for line in text.lines() {
            // A folder path may contain spaces: it is the rest of the line.
            if let Some(folder) = line.strip_prefix("exclude ").map(str::trim).filter(|f| valid_folder(f)) {
                drives.excludes.push(folder.to_string());
                continue;
            }
            if line.trim() == "nohidden" {
                drives.no_hidden = true;
                continue;
            }
            let mut words = line.split_whitespace();
            let list = match words.next() {
                Some("skip") => &mut drives.skip,
                Some("nolive") => &mut drives.nolive,
                Some("device") => &mut drives.devices,
                Some("include") => &mut drives.include,
                Some("manual") => {
                    drives.manual = true;
                    continue;
                }
                _ => continue, // comments, blank lines, things from the future
            };
            list.extend(words.next().filter(|d| valid_device(d)).map(String::from));
        }
        drives
    }

    fn to_text(&self) -> String {
        let mut text = String::from("# Fast search daemon: per-drive settings (written by ffd --configure).\n");
        if self.manual {
            text += "manual\n";
        }
        if self.no_hidden {
            text += "nohidden\n";
        }
        for folder in &self.excludes {
            text += &format!("exclude {folder}\n");
        }
        let lists = [("skip", &self.skip), ("nolive", &self.nolive), ("device", &self.devices), ("include", &self.include)];
        for (key, list) in lists {
            for device in list {
                text += &format!("{key} {device}\n");
            }
        }
        text
    }
}

/// A folder path we are willing to store: absolute, on one line.
fn valid_folder(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(char::is_control)
}

/// A device path we are willing to store and act on.
fn valid_device(path: &str) -> bool {
    path.starts_with("/dev/") && path.len() > 5 && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b))
}

/// What to index and watch, given the settings and what is mounted now.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    /// Mount points left out of the walk (their drive is skipped).
    excludes: Vec<PathBuf>,
    /// Filesystems to watch for changes.
    watch: Vec<Mount>,
    /// Configured drives that are present and not mounted: read directly.
    raw: Vec<String>,
}

fn plan(mounts: Vec<Mount>, drives: &Drives, present: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    for device in &drives.devices {
        if !mounts.iter().any(|m| &m.source == device) && present(device) {
            plan.raw.push(device.clone());
        }
    }
    for mount in mounts {
        let left_out = drives.manual && !drives.include.contains(&mount.source);
        if left_out || drives.skip.contains(&mount.source) {
            plan.excludes.push(mount.point);
        } else if !drives.nolive.contains(&mount.source) {
            plan.watch.push(mount);
        }
    }
    plan
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

fn group_id(name: &str) -> Result<u32, String> {
    let text = fs::read_to_string("/etc/group").map_err(|e| format!("/etc/group: {e}"))?;
    text.lines()
        .find_map(|line| {
            let mut f = line.split(':');
            (f.next()? == name).then(|| f.nth(1)?.parse().ok())?
        })
        .ok_or_else(|| format!("no such group: {name}"))
}

fn run() -> Result<(), String> {
    let mut roots = Vec::new();
    let mut excludes = Vec::new();
    let mut cfg = Config {
        opts: BuildOptions::new(["/"]),
        output: ff_core::SYSTEM_DB.into(),
        group: None,
        interval: Duration::from_secs(2),
        drives: Drives::parse(&fs::read_to_string(CONFIG_FILE).unwrap_or_default()),
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return Ok(());
            }
            Some("--install-service") => return install_service(&value("--install-service")?.to_string_lossy()),
            Some("--uninstall-service") => return uninstall_service(),
            Some("--configure") => return configure(args.map(|a| a.to_string_lossy().into_owned()).collect()),
            Some(f @ ("--skip" | "--nolive" | "--device")) => {
                let device = value(f)?.to_string_lossy().into_owned();
                if !valid_device(&device) {
                    return Err(format!("{f}: {device} is not a device path like /dev/sdb1"));
                }
                match f {
                    "--skip" => cfg.drives.skip.push(device),
                    "--nolive" => cfg.drives.nolive.push(device),
                    _ => cfg.drives.devices.push(device),
                }
            }
            Some(f @ ("-g" | "--group")) => cfg.group = Some(group_id(&value(f)?.to_string_lossy())?),
            Some(f @ ("-x" | "--exclude")) => excludes.push(PathBuf::from(value(f)?)),
            Some(f @ ("-o" | "--output")) => cfg.output = value(f)?.into(),
            Some(f @ ("-i" | "--interval")) => {
                let secs = value(f)?.to_str().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s >= 0.1);
                cfg.interval = Duration::from_secs_f64(secs.ok_or("--interval needs a number of seconds >= 0.1")?);
            }
            Some(f) if f.starts_with('-') => return Err(format!("unknown option {f} (see ffd --help)")),
            _ => roots.push(PathBuf::from(arg)),
        }
    }
    if !roots.is_empty() {
        cfg.opts = BuildOptions::new(roots);
    }
    cfg.opts.excludes.extend(excludes);
    cfg.opts.excludes.extend(cfg.drives.excludes.iter().map(PathBuf::from));
    cfg.opts.skip_hidden = cfg.drives.no_hidden;
    serve(&cfg)
}

/// The mount table as (watchable mounts, settings applied): the state
/// whose changes mean a drive came or went.
fn current_plan(cfg: &Config) -> Result<Plan, String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
    let roots: Vec<PathBuf> = cfg.opts.roots.iter().filter_map(|r| fs::canonicalize(r).ok()).collect();
    let mounts = watchable_mounts(&mountinfo, &roots, &cfg.opts.excludes);
    Ok(plan(mounts, &cfg.drives, |device| Path::new(device).exists()))
}

fn serve(cfg: &Config) -> Result<(), String> {
    // Files we create are private to root and the reader group.
    sys::umask(0o027);
    let group = sys::fanotify_init().map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => "fanotify needs root; start ffd with sudo or as a system service".to_string(),
        _ => format!("fanotify_init: {e}"),
    })?;

    prepare_output_dir(cfg)?;
    let log_file = cfg.output.with_file_name("ffd.log");
    let _ = fs::remove_file(&log_file);
    let _ = LOG_FILE.set(log_file);

    // A private mount table lets us mount a filesystem's top level for
    // watching (see `watch_top_level`) without anyone else seeing it.
    let private_mounts = sys::unshare_mounts().is_ok() && command_ok("mount", &["--make-rslave", "/"]);
    if !private_mounts {
        log("could not get a private mount table; btrfs subvolume fallback disabled");
    }

    // Open folders of the watched filesystems, shared with the thread that
    // turns events into paths; replaced when drives come and go.
    let mount_dirs = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    {
        let (group, mount_dirs) = (group.try_clone().map_err(|e| e.to_string())?, mount_dirs.clone());
        std::thread::spawn(move || read_events(group, mount_dirs, tx));
    }

    let scan = |why: &str, plan: &Plan| -> Result<LiveIndex, String> {
        let start = Instant::now();
        // Start watching before the scan so nothing that changes during
        // it is missed; those events are applied once the scan is done.
        *mount_dirs.lock().unwrap() = watch_filesystems(&group, &plan.watch, private_mounts);
        let mut opts = BuildOptions::new(cfg.opts.roots.clone());
        opts.excludes = cfg.opts.excludes.iter().chain(&plan.excludes).cloned().collect();
        opts.skip_hidden = cfg.opts.skip_hidden;
        let mut index = Index::build(&opts).map_err(|e| e.to_string())?;
        // Drives read directly are read again on every scan: keeping a
        // copy would cost as much memory as the drive's whole index, and
        // reading one takes seconds.
        for device in &plan.raw {
            let read_start = Instant::now();
            match ff_core::rawfs::scan(Path::new(device), device.as_bytes()) {
                Ok(read) => {
                    log(format!("read {device} directly: {} entries in {:.2}s", read.len(), read_start.elapsed().as_secs_f64()));
                    index.append(&read);
                }
                Err(e) => log(format!("cannot read {device} directly: {e}")),
            }
        }
        index.shrink();
        for skipped in &plan.excludes {
            log(format!("not indexing {} (drive is switched off)", skipped.display()));
        }
        log(format!("{why}: {} entries in {:.2}s", index.len(), start.elapsed().as_secs_f64()));
        // Nothing changes on those drives, so they need no live bookkeeping.
        let fixed = plan.raw.iter().map(|device| device.as_bytes().to_vec()).collect();
        Ok(LiveIndex::with_fixed_roots(index, fixed))
    };

    let mut current = current_plan(cfg)?;
    let mut live = scan("indexed", &current)?;
    publish(cfg, live.index())?;
    log(format!("publishing to {}", cfg.output.display()));

    let own_dir = cfg.output.parent().map(|p| p.as_os_str().as_bytes().to_vec());
    loop {
        // Sleep until a file changes, or look at the drives every few seconds.
        let first = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("the event reader stopped".into()),
        };
        // A drive was mounted, unmounted, plugged in or removed: start over.
        let now = current_plan(cfg)?;
        if now != current {
            current = now;
            live = scan("drives changed, rescanned", &current)?;
            if let Err(e) = publish(cfg, live.index()) {
                log(e);
            }
            continue;
        }
        let Some(first) = first else { continue };
        // Gather a batch of changes.
        std::thread::sleep(cfg.interval);
        let mut changes = HashSet::new();
        let mut overflow = false;
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                // Our own index file being rewritten is not news.
                Msg::Changed(dir, _) if Some(&dir) == own_dir.as_ref() => {}
                Msg::Changed(dir, name) => {
                    changes.insert((dir, name));
                }
                Msg::Overflow => overflow = true,
            }
        }
        let changed = if overflow {
            live = scan("events were dropped, rescanned", &current)?;
            true
        } else {
            live.apply(&changes)
        };
        if changed {
            if let Err(e) = publish(cfg, live.index()) {
                log(e);
            }
        }
    }
}

/// `--configure`: saves per-drive settings to /etc/ffd.conf and restarts
/// the service so they take effect. Needs root.
fn configure(args: Vec<String>) -> Result<(), String> {
    let mut drives = Drives::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        if flag == "--manual" {
            drives.manual = true;
            continue;
        }
        if flag == "--no-hidden" {
            drives.no_hidden = true;
            continue;
        }
        if flag == "--exclude" {
            let folder = args.next().filter(|f| valid_folder(f)).ok_or("--exclude needs an absolute folder path")?;
            drives.excludes.push(folder);
            continue;
        }
        let device = args.next().filter(|d| valid_device(d)).ok_or(format!("{flag} needs a device path like /dev/sdb1"))?;
        match flag.as_str() {
            "--include" => drives.include.push(device),
            "--skip" => drives.skip.push(device),
            "--nolive" => drives.nolive.push(device),
            "--device" => drives.devices.push(device),
            _ => return Err(format!("--configure: unknown setting {flag}")),
        }
    }
    fs::write(CONFIG_FILE, drives.to_text()).map_err(|e| format!("{CONFIG_FILE}: {e} (run this with sudo)"))?;
    fs::set_permissions(CONFIG_FILE, fs::Permissions::from_mode(0o644)).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
    println!("saved {CONFIG_FILE}");
    // Only restart a service that is running; ignore one that is not installed.
    let _ = systemctl(&["try-restart", "ffd"]);
    Ok(())
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program).args(args).status().is_ok_and(|s| s.success())
}

/// Creates the folder the index is published in, readable by the group.
fn prepare_output_dir(cfg: &Config) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", cfg.output.display());
    if let Some(dir) = cfg.output.parent().filter(|d| !d.exists()) {
        fs::create_dir_all(dir).map_err(fail)?;
        if let Some(gid) = cfg.group {
            // Set-group-id directory: files created inside inherit the
            // group, so the index is never briefly unreadable.
            std::os::unix::fs::chown(dir, None, Some(gid)).map_err(fail)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o2750)).map_err(fail)?;
        }
    }
    Ok(())
}

/// Writes the index where readers look for it, readable by the group.
fn publish(cfg: &Config, index: &Index) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", cfg.output.display());
    prepare_output_dir(cfg)?;
    index.save(&cfg.output).map_err(fail)?;
    if let Some(gid) = cfg.group {
        std::os::unix::fs::chown(&cfg.output, None, Some(gid)).map_err(fail)?;
    }
    Ok(())
}

/// A mounted filesystem that holds indexed files and can be watched.
#[derive(Debug, PartialEq)]
struct Mount {
    point: PathBuf,
    fstype: String,
    /// The device, e.g. `/dev/sda2`.
    source: String,
}

/// Picks from the text of /proc/self/mountinfo the mounts worth watching:
/// real filesystems that hold part of an indexed root and are not excluded.
fn watchable_mounts(mountinfo: &str, roots: &[PathBuf], excludes: &[PathBuf]) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in mountinfo.lines() {
        // "id parent major:minor root MOUNTPOINT options... - FSTYPE source ..."
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let mut right = right.split(' ');
        let (Some(point), Some(fstype), Some(source)) = (left.split(' ').nth(4), right.next(), right.next()) else {
            continue;
        };
        let point = PathBuf::from(std::ffi::OsString::from_vec(unescape(point)));
        // Relevant if it holds indexed files: inside a root, or a root is inside it.
        let relevant = roots.iter().any(|r| point.starts_with(r) || r.starts_with(&point));
        let excluded = excludes.iter().any(|x| point.starts_with(x));
        if relevant && !excluded && WATCHABLE.contains(&fstype) {
            mounts.push(Mount { point, fstype: fstype.into(), source: source.into() });
        }
    }
    mounts
}

/// Puts a filesystem-wide watch on each of `mounts`. Returns an open
/// folder per filesystem, used to turn file handles into paths.
fn watch_filesystems(group: &File, mounts: &[Mount], private_mounts: bool) -> Vec<File> {
    let mask = sys::FAN_CREATE
        | sys::FAN_DELETE
        | sys::FAN_MOVED_FROM
        | sys::FAN_MOVED_TO
        | sys::FAN_CLOSE_WRITE
        | sys::FAN_ONDIR;
    let mut dirs = Vec::new();
    // Per device: did any of its mounts accept a watch?
    let mut devices: Vec<(&str, &str, bool)> = Vec::new();
    for Mount { point, fstype, source } in mounts {
        let watched = match sys::fanotify_mark_filesystem(group, mask, point) {
            Ok(()) => {
                log(format!("watching {} ({fstype})", point.display()));
                true
            }
            Err(e) => {
                log(format!("no direct watch on {} ({fstype}): {e}", point.display()));
                false
            }
        };
        match devices.iter_mut().find(|d| d.0 == source) {
            Some(device) => device.2 |= watched,
            None => devices.push((source, fstype, watched)),
        }
        if let Ok(dir) = File::open(point) {
            dirs.push(dir);
        }
    }
    for (n, (source, fstype, watched)) in devices.iter().enumerate() {
        if *watched {
            continue;
        }
        if *fstype == "btrfs" && private_mounts {
            watch_top_level(group, mask, source, n);
        } else {
            log(format!("{source} ({fstype}) is NOT watched; changes there will be missed"));
        }
    }
    dirs
}

/// btrfs only accepts a filesystem-wide watch on its top-level volume, and
/// systems like openSUSE mount only subvolumes of it. Mount the top level
/// privately (our mount table is our own) and watch that: the watch then
/// covers every subvolume of the device.
fn watch_top_level(group: &File, mask: u64, source: &str, n: usize) {
    let dir = PathBuf::from(format!("/run/ffd-mounts/{n}"));
    // Already mounted by an earlier scan: the watch is still in place.
    if fs::read_to_string("/proc/self/mountinfo").is_ok_and(|m| m.contains(&format!(" {} ", dir.display()))) {
        return;
    }
    let mounted = fs::create_dir_all(&dir).is_ok()
        && command_ok("mount", &["-t", "btrfs", "-o", "subvolid=5,noatime", source, &dir.to_string_lossy()]);
    if !mounted {
        return log(format!("{source}: could not mount the btrfs top level; changes there will be missed"));
    }
    match sys::fanotify_mark_filesystem(group, mask, &dir) {
        Ok(()) => log(format!("watching {source} (btrfs, all subvolumes, via its top level)")),
        Err(e) => log(format!("{source}: watching the btrfs top level failed: {e}; changes there will be missed")),
    }
}

/// Decodes the `\040`-style octal escapes used in /proc/self/mountinfo.
fn unescape(field: &str) -> Vec<u8> {
    let b = field.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let octal = b.get(i + 1..i + 4).filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        if let Some(d) = octal {
            out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// One decoded fanotify event.
#[derive(Debug, PartialEq)]
enum Event<'a> {
    /// The kernel dropped events.
    Overflow,
    /// Entry `name` changed in the directory identified by `handle`
    /// (the raw `struct file_handle` bytes).
    Entry { handle: &'a [u8], name: &'a [u8] },
}

/// Decodes the events in one read from a fanotify descriptor. Anything
/// malformed or of a kind we did not ask for is skipped.
fn parse_events(buf: &[u8]) -> Vec<Event<'_>> {
    let u16at = |b: &[u8], i: usize| u16::from_ne_bytes([b[i], b[i + 1]]) as usize;
    let u32at = |b: &[u8], i: usize| u32::from_ne_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let mut events = Vec::new();
    // Each event: a 24-byte header (length, version, header length,
    // 64-bit mask, fd, pid) followed by information records.
    let mut at = 0;
    while at + 24 <= buf.len() {
        let (event_len, header_len) = (u32at(buf, at), u16at(buf, at + 6));
        if event_len < 24 || at + event_len > buf.len() {
            break;
        }
        let event = &buf[at..at + event_len];
        at += event_len;
        let mask = u64::from_ne_bytes(event[8..16].try_into().unwrap());
        if mask & sys::FAN_Q_OVERFLOW != 0 {
            events.push(Event::Overflow);
            continue;
        }
        // Record: type u8, pad u8, length u16, fsid (8 bytes), then a
        // file handle (size u32, type i32, bytes) and a NUL-ended name.
        let mut rec = header_len;
        while rec + 4 <= event.len() {
            let (kind, len) = (event[rec], u16at(event, rec + 2));
            if len < 4 || rec + len > event.len() {
                break;
            }
            let record = &event[rec..rec + len];
            rec += len;
            const DIR_HANDLE_AND_NAME: u8 = 2;
            if kind != DIR_HANDLE_AND_NAME || record.len() < 20 {
                continue;
            }
            let handle_end = 20 + u32at(record, 12);
            let (Some(handle), Some(name)) = (record.get(12..handle_end), record.get(handle_end..)) else {
                continue;
            };
            let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
            events.push(Event::Entry { handle, name });
        }
    }
    events
}

/// The (directory, name) pair to re-check for an event in directory `dir`.
/// A name of "." means the directory itself, reported under its parent.
fn changed_entry(dir: Vec<u8>, name: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if name != b"." && !name.is_empty() {
        return Some((dir, name.to_vec()));
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(&dir));
    Some((path.parent()?.as_os_str().as_bytes().to_vec(), path.file_name()?.as_bytes().to_vec()))
}

/// Reads fanotify events forever, turning each into a message. Runs on its
/// own thread so directory handles are resolved to paths promptly, before
/// the directory can go away.
fn read_events(mut group: File, mounts: Arc<Mutex<Vec<File>>>, tx: mpsc::Sender<Msg>) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match group.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log(format!("reading events failed: {e}"));
                return; // dropping `tx` stops the main loop
            }
        };
        for event in parse_events(&buf[..n]) {
            let msg = match event {
                Event::Overflow => Msg::Overflow,
                Event::Entry { handle, name } => {
                    // No path: the directory is already gone, or it is
                    // not under a mount we index.
                    let Some(dir) = mounts.lock().unwrap().iter().find_map(|m| handle_path(m, handle)) else { continue };
                    let Some((dir, name)) = changed_entry(dir.into_os_string().into_vec(), name) else { continue };
                    Msg::Changed(dir, name)
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    }
}

/// Path of the directory a file handle refers to, as seen through mount
/// `mount`, or `None` if it cannot be reached that way.
///
/// A handle opens through any mount of its filesystem, but the path is
/// only real through the mount that actually contains the directory
/// (think of btrfs subvolumes mounted at / and /home). So the path is
/// checked: it must lead back to the very same directory.
fn handle_path(mount: &File, handle: &[u8]) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dir = sys::open_by_handle(mount, handle).ok()?;
    let path = fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&dir))).ok()?;
    let (by_handle, by_path) = (dir.metadata().ok()?, fs::metadata(&path).ok()?);
    ((by_handle.dev(), by_handle.ino()) == (by_path.dev(), by_path.ino())).then_some(path)
}

/// `--install-service GROUP`: copies this binary to /usr/local/bin, writes
/// a systemd unit that runs it for GROUP, and starts it.
fn install_service(group: &str) -> Result<(), String> {
    group_id(group)?;
    if !group.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(format!("unsupported group name: {group}"));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = Path::new("/usr/local/bin/ffd");
    let fail = |what: &Path, e: io::Error| format!("{}: {e} (run this with sudo)", what.display());
    if exe != target {
        // Copy to a temporary name and rename, so a running ffd is not
        // overwritten in place.
        let tmp = target.with_extension("new");
        fs::copy(&exe, &tmp).map_err(|e| fail(&tmp, e))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| fail(&tmp, e))?;
        fs::rename(&tmp, target).map_err(|e| fail(target, e))?;
    }
    let unit = Path::new("/etc/systemd/system/ffd.service");
    let text = format!(
        "[Unit]\nDescription=Fast search index daemon (ffd)\n\n\
         [Service]\nExecStart={} --group {group}\nRestart=on-failure\nNice=10\nNoNewPrivileges=yes\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        target.display()
    );
    fs::write(unit, text).map_err(|e| fail(unit, e))?;
    println!("installed {} and {}", target.display(), unit.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "ffd"])?;
    systemctl(&["restart", "ffd"])?;
    println!("the ffd service is running");
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("systemctl {} failed ({s})", args.join(" "))),
        Err(e) => Err(format!("cannot run systemctl: {e}")),
    }
}

/// `--uninstall-service`: stops the service and removes what
/// `--install-service` created.
fn uninstall_service() -> Result<(), String> {
    let _ = systemctl(&["disable", "--now", "ffd"]); // may not be installed
    for file in ["/etc/systemd/system/ffd.service", "/usr/local/bin/ffd"] {
        match fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(format!("{file}: {e} (run this with sudo)")),
            _ => {}
        }
    }
    let _ = fs::remove_dir_all("/run/ff");
    systemctl(&["daemon-reload"])?;
    println!("the ffd service is removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one fanotify event as the kernel lays it out.
    fn event(mask: u64, records: &[(u8, &[u8], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for &(kind, handle_bytes, name) in records {
            let mut rec = vec![kind, 0, 0, 0];
            rec.extend([0xAA; 8]); // fsid
            rec.extend((handle_bytes.len() as u32).to_ne_bytes());
            rec.extend(1i32.to_ne_bytes()); // handle type
            rec.extend(handle_bytes);
            rec.extend(name);
            rec.push(0);
            rec.resize(rec.len().next_multiple_of(4), 0); // the kernel pads records
            let len = rec.len() as u16;
            rec[2..4].copy_from_slice(&len.to_ne_bytes());
            body.extend(rec);
        }
        let mut ev = Vec::new();
        ev.extend((24 + body.len() as u32).to_ne_bytes());
        ev.extend([3, 0]); // version, reserved
        ev.extend(24u16.to_ne_bytes());
        ev.extend(mask.to_ne_bytes());
        ev.extend((-1i32).to_ne_bytes()); // no fd in this mode
        ev.extend(0i32.to_ne_bytes()); // pid
        ev.extend(body);
        ev
    }

    /// The handle slice the parser should hand to open_by_handle_at.
    fn handle(bytes: &[u8]) -> Vec<u8> {
        let mut h = (bytes.len() as u32).to_ne_bytes().to_vec();
        h.extend(1i32.to_ne_bytes());
        h.extend(bytes);
        h
    }

    #[test]
    fn decodes_events_with_names() {
        let mut buf = event(sys::FAN_CREATE, &[(2, b"HANDLE-1", b"new.txt")]);
        buf.extend(event(sys::FAN_DELETE | sys::FAN_ONDIR, &[(2, b"H2", b"old dir")]));
        let (h1, h2) = (handle(b"HANDLE-1"), handle(b"H2"));
        assert_eq!(
            parse_events(&buf),
            [Event::Entry { handle: &h1, name: b"new.txt" }, Event::Entry { handle: &h2, name: b"old dir" }]
        );
    }

    #[test]
    fn decodes_overflow_and_skips_other_records() {
        let mut buf = event(sys::FAN_Q_OVERFLOW, &[]);
        // Record kinds 1 (file id) and 3 (directory id, no name) are not ours.
        buf.extend(event(sys::FAN_CREATE, &[(1, b"FID", b""), (3, b"DFID", b""), (2, b"H", b"x")]));
        let h = handle(b"H");
        assert_eq!(parse_events(&buf), [Event::Overflow, Event::Entry { handle: &h, name: b"x" }]);
    }

    #[test]
    fn survives_truncated_and_garbage_input() {
        let good = event(sys::FAN_CREATE, &[(2, b"HANDLE", b"file")]);
        for cut in 0..good.len() {
            assert!(parse_events(&good[..cut]).is_empty(), "cut at {cut}");
        }
        assert!(parse_events(&[0xFF; 64]).is_empty());
        assert!(parse_events(&[0; 64]).is_empty());
        // A record claiming a handle longer than the record is skipped.
        let mut bad = event(sys::FAN_CREATE, &[(2, b"HANDLE", b"file")]);
        bad[24 + 12..24 + 16].copy_from_slice(&1000u32.to_ne_bytes());
        assert!(parse_events(&bad).is_empty());
    }

    #[test]
    fn dot_means_the_directory_itself() {
        let pair = |d: &str, n: &str| Some((d.as_bytes().to_vec(), n.as_bytes().to_vec()));
        assert_eq!(changed_entry(b"/home/me".to_vec(), b"notes.txt"), pair("/home/me", "notes.txt"));
        assert_eq!(changed_entry(b"/home/me/docs".to_vec(), b"."), pair("/home/me", "docs"));
        assert_eq!(changed_entry(b"/home/me/docs".to_vec(), b""), pair("/home/me", "docs"));
        assert_eq!(changed_entry(b"/".to_vec(), b"."), None); // the root has no parent
    }

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(unescape("/mnt/my\\040disk"), b"/mnt/my disk");
        assert_eq!(unescape("/a\\011b\\134c"), b"/a\tb\\c");
        assert_eq!(unescape("/plain"), b"/plain");
        assert_eq!(unescape("/trailing\\04"), b"/trailing\\04"); // not a full escape
    }

    // A mount table like openSUSE's: one btrfs device, every folder a
    // subvolume, plus the usual pseudo filesystems.
    const MOUNTINFO: &str = "\
23 46 0:22 / /proc rw,nosuid shared:13 - proc proc rw
24 46 0:23 / /sys rw,nosuid shared:2 - sysfs sysfs rw,seclabel
25 46 0:6 / /dev rw,nosuid shared:9 - devtmpfs devtmpfs rw,seclabel,size=4096k
27 46 0:31 / /run rw,nosuid,nodev shared:11 - tmpfs tmpfs rw,seclabel,mode=755
46 1 0:42 /@/.snapshots/1/snapshot / rw,relatime shared:1 - btrfs /dev/nvme1n1p3 rw,subvolid=264
44 46 0:51 /@/home /home rw,relatime shared:55 - btrfs /dev/nvme1n1p3 rw,subvolid=262
45 46 0:50 /@/opt /opt rw,relatime shared:57 - btrfs /dev/nvme1n1p3 rw,subvolid=261
47 46 0:41 /@/.snapshots /.snapshots rw,relatime shared:59 - btrfs /dev/nvme1n1p3 rw,subvolid=265
48 46 0:47 / /tmp rw,nosuid,nodev shared:61 - tmpfs tmpfs rw,seclabel
49 46 259:1 / /boot/efi rw,relatime shared:63 - vfat /dev/nvme1n1p1 rw,fmask=0022
50 27 0:78 / /run/user/1000 rw,nosuid,nodev shared:65 - tmpfs tmpfs rw,seclabel
51 50 0:80 / /run/user/1000/gvfs rw,nosuid,nodev shared:67 - fuse.gvfsd-fuse gvfsd-fuse rw
52 46 8:17 / /mnt/my\\040disk rw,relatime shared:69 - ext4 /dev/sdb1 rw
53 46 0:90 / /mnt/nas rw,relatime shared:71 - nfs4 nas:/export rw
malformed line without the separator
";

    fn points(mounts: &[Mount]) -> Vec<&str> {
        mounts.iter().map(|m| m.point.to_str().unwrap()).collect()
    }

    #[test]
    fn picks_real_filesystems_under_the_roots() {
        let excludes = ["/proc", "/sys", "/dev", "/run", "/.snapshots"].map(PathBuf::from);
        let all = watchable_mounts(MOUNTINFO, &["/".into()], &excludes);
        // Pseudo, network and FUSE filesystems and excluded folders are out.
        assert_eq!(points(&all), ["/", "/home", "/opt", "/tmp", "/boot/efi", "/mnt/my disk"]);
        assert_eq!(all[1], Mount { point: "/home".into(), fstype: "btrfs".into(), source: "/dev/nvme1n1p3".into() });
        assert_eq!(all[5].source, "/dev/sdb1");

        // Indexing only a folder: its own mount, mounts below it, and the
        // mount that contains it.
        let home = watchable_mounts(MOUNTINFO, &["/home/me/work".into()], &excludes);
        assert_eq!(points(&home), ["/", "/home"]);
        let mnt = watchable_mounts(MOUNTINFO, &["/mnt".into()], &excludes);
        assert_eq!(points(&mnt), ["/", "/mnt/my disk"]);

        // Without the default excludes, /run's tmpfs mounts are included.
        let none = watchable_mounts(MOUNTINFO, &["/run".into()], &[]);
        assert_eq!(points(&none), ["/run", "/", "/run/user/1000"]);
    }

    #[test]
    fn drive_settings_round_trip() {
        let text = "# comment\nskip /dev/sdb1\nnolive /dev/sdc1\ndevice /dev/nvme0n1p2\nskip not-a-device\nfuture thing\nskip /dev/bad;rm\n";
        let drives = Drives::parse(text);
        assert_eq!(drives.skip, ["/dev/sdb1"]);
        assert_eq!(drives.nolive, ["/dev/sdc1"]);
        assert_eq!(drives.devices, ["/dev/nvme0n1p2"]);
        assert_eq!(Drives::parse(&drives.to_text()), drives);
        let more = Drives::parse("nohidden\nexclude /home/me/My Videos\nexclude relative/path\n");
        assert_eq!((more.no_hidden, &more.excludes[..]), (true, &["/home/me/My Videos".to_string()][..]));
        assert_eq!(Drives::parse(&more.to_text()), more);
        assert!(valid_device("/dev/mapper/vg-home"));
        assert!(!valid_device("/dev/"));
        assert!(!valid_device("/etc/passwd"));
        assert!(!valid_device("/dev/sd a"));
    }

    #[test]
    fn plans_what_to_index_and_watch() {
        let mount = |point: &str, source: &str| Mount { point: point.into(), fstype: "ext4".into(), source: source.into() };
        let mounts = || vec![mount("/", "/dev/sda1"), mount("/data", "/dev/sdb1"), mount("/media/usb", "/dev/sdc1")];
        let drives = Drives {
            skip: vec!["/dev/sdb1".into()],
            nolive: vec!["/dev/sdc1".into()],
            devices: vec!["/dev/sdd1".into(), "/dev/sde1".into(), "/dev/sdc1".into()],
            ..Default::default()
        };
        // sdd1 is plugged in and not mounted; sde1 is absent; sdc1 is mounted.
        let planned = plan(mounts(), &drives, |device| device != "/dev/sde1");
        assert_eq!(planned.excludes, [PathBuf::from("/data")]);
        assert_eq!(planned.watch, [mount("/", "/dev/sda1")]);
        assert_eq!(planned.raw, ["/dev/sdd1"]);

        // Unplugging sdd1 or unmounting /data changes the plan, which is
        // what triggers a rescan.
        assert_ne!(plan(mounts(), &drives, |_| false), planned);
        let mut fewer = mounts();
        fewer.remove(1);
        assert_ne!(plan(fewer, &drives, |device| device != "/dev/sde1"), planned);
        // Manual mode: only included drives are indexed, even when mounted.
        let manual = Drives { manual: true, include: vec!["/dev/sda1".into()], ..Default::default() };
        let picked = plan(mounts(), &manual, |_| true);
        assert_eq!(picked.watch, [mount("/", "/dev/sda1")]);
        assert_eq!(picked.excludes, [PathBuf::from("/data"), PathBuf::from("/media/usb")]);
        assert_eq!(Drives::parse(&manual.to_text()), manual);
        // No settings: everything mounted is indexed and watched.
        let plain = plan(mounts(), &Drives::default(), |_| true);
        assert_eq!((plain.excludes.len(), plain.watch.len(), plain.raw.len()), (0, 3, 0));
    }
}
