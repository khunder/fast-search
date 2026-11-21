//! Everything behind the Options menu: saved preferences, the launcher
//! entry, the global shortcut and the live-updates service.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Command, Stdio};
#[cfg(not(windows))]
use std::time::SystemTime;

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// `$VAR`, else `~/fallback`.
#[cfg(not(windows))]
fn home_dir(var: &str, fallback: &str) -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(fallback))
}

/// The program to start for "Fast search": the AppImage file when we run
/// from one (the path inside it only exists while it runs), else this
/// executable.
#[cfg(not(windows))]
fn launch_path() -> io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        Some(appimage) => Ok(appimage.into()),
        None => {
            // A program replaced while it runs (rebuilt, upgraded) is
            // reported as "path (deleted)": the path is still the one to start.
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let exe = std::env::current_exe()?.into_os_string().into_vec();
            let exe = exe.strip_suffix(b" (deleted)").unwrap_or(&exe);
            Ok(std::ffi::OsStr::from_bytes(exe).into())
        }
    }
}

/// True if the desktop asks applications for a dark appearance.
#[cfg(not(windows))]
pub fn desktop_prefers_dark() -> bool {
    let scheme = Command::new("gsettings").args(["get", "org.gnome.desktop.interface", "color-scheme"]).output();
    scheme.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("dark"))
}

// ---- saved preferences ---------------------------------------------------

/// Preferences kept in `~/.config/ff/gui.conf` as `key=value` lines.
pub struct Config {
    /// Zoom chosen by the user; `None` follows the desktop scale.
    pub scale: Option<f32>,
    pub own_title_bar: bool,
    /// How searches are read by default.
    pub regex: bool,
    pub whole_word: bool,
    /// Letter case: "smart", "sensitive" or "insensitive".
    pub case: String,
    /// Every other setting, by name.
    pub more: std::collections::BTreeMap<String, String>,
}

impl Config {
    fn path() -> io::Result<PathBuf> {
        Ok(ff_core::os::config_dir().ok_or_else(|| other("HOME is not set"))?.join("ff").join("gui.conf"))
    }

    pub fn load() -> Config {
        let mut config = Config {
            scale: None,
            own_title_bar: true,
            regex: false,
            whole_word: false,
            case: "smart".into(),
            more: Default::default(),
        };
        let text = Config::path().and_then(fs::read_to_string).unwrap_or_default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("scale", v)) => config.scale = v.trim().parse().ok(),
                Some(("title_bar", v)) => config.own_title_bar = v.trim() != "system",
                Some(("regex", v)) => config.regex = v.trim() == "on",
                Some(("whole_word", v)) => config.whole_word = v.trim() == "on",
                Some(("case", v)) => config.case = v.trim().to_string(),
                Some((key, v)) => drop(config.more.insert(key.trim().to_string(), v.trim().to_string())),
                _ => {}
            }
        }
        config
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Config::path()?;
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let on = |b: bool| if b { "on" } else { "off" };
        let mut text = format!("title_bar={}\n", if self.own_title_bar { "own" } else { "system" });
        text.push_str(&format!("regex={}\nwhole_word={}\ncase={}\n", on(self.regex), on(self.whole_word), self.case));
        if let Some(scale) = self.scale {
            text.push_str(&format!("scale={scale}\n"));
        }
        for (key, value) in &self.more {
            text.push_str(&format!("{key}={value}\n"));
        }
        fs::write(path, text)
    }
}

// ---- applications menu entry ----------------------------------------------

#[cfg(not(windows))]
const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
<path d="M5 50A51 51 0 0 1 95 50A51 51 0 0 1 5 50Z" fill="#f08a1c"/>
<path d="M11.34 50A45.5 45.5 0 0 1 88.66 50A45.5 45.5 0 0 1 11.34 50Z" fill="#fdfdfd"/>
<circle cx="50" cy="50" r="17" fill="#c96a12"/>
<circle cx="50" cy="50" r="8.5" fill="#2b2118"/>
<circle cx="55.5" cy="44.5" r="3.5" fill="#fff"/>
</svg>
"##;

/// (desktop entry, icon file, icon theme folder).
#[cfg(not(windows))]
fn launcher_paths() -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let data = home_dir("XDG_DATA_HOME", ".local/share")?;
    let theme = data.join("icons/hicolor");
    Ok((data.join("applications/ff-gui.desktop"), theme.join("scalable/apps/ff-gui.svg"), theme))
}

#[cfg(not(windows))]
pub fn launcher_installed() -> bool {
    launcher_paths().is_ok_and(|(entry, ..)| entry.exists())
}

/// A stale icon cache would hide a new icon: rebuild it if we can, and
/// mark the theme folder as changed either way.
#[cfg(not(windows))]
fn refresh_icon_cache(theme: &Path) {
    let _ = Command::new("gtk-update-icon-cache").args(["-f", "-t", "-q"]).arg(theme).status();
    let _ = fs::File::open(theme).and_then(|dir| dir.set_modified(SystemTime::now()));
}

/// Writes a desktop entry and icon so the desktop lists "Fast search"
/// with its icon in the applications menu and the dock.
#[cfg(not(windows))]
pub fn install_launcher() -> io::Result<PathBuf> {
    let (entry, icon, theme) = launcher_paths()?;
    for file in [&entry, &icon] {
        fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
    }
    fs::write(&icon, ICON_SVG)?;
    refresh_icon_cache(&theme);
    let exe = launch_path()?;
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Fast search\nComment=Find files instantly by name\n\
         Exec=\"{}\"\nIcon=ff-gui\nTerminal=false\nCategories=Utility;FileTools;\nStartupWMClass=ff-gui\n",
        exe.display()
    );
    fs::write(&entry, text)?;
    Ok(entry)
}

#[cfg(not(windows))]
pub fn remove_launcher() -> io::Result<()> {
    let (entry, icon, theme) = launcher_paths()?;
    for file in [entry, icon] {
        match fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    refresh_icon_cache(&theme);
    Ok(())
}

// ---- global shortcut (GNOME) ------------------------------------------------

#[cfg(not(windows))]
const KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
#[cfg(not(windows))]
const KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/ff/";
/// The keys used until the user chooses others.
#[cfg(not(windows))]
pub const DEFAULT_SHORTCUT: &str = "Alt+E";

/// Keys with a name: how we write them, and their GNOME (X keysym) name.
const NAMED_KEYS: [(&str, &str); 26] = [
    ("Space", "space"),
    ("Enter", "Return"),
    ("Tab", "Tab"),
    ("Esc", "Escape"),
    ("Home", "Home"),
    ("End", "End"),
    ("Insert", "Insert"),
    ("Delete", "Delete"),
    ("PageUp", "Page_Up"),
    ("PageDown", "Page_Down"),
    ("Up", "Up"),
    ("Down", "Down"),
    ("Left", "Left"),
    ("Right", "Right"),
    ("Print", "Print"),
    (",", "comma"),
    (".", "period"),
    ("/", "slash"),
    (";", "semicolon"),
    ("'", "apostrophe"),
    ("[", "bracketleft"),
    ("]", "bracketright"),
    ("-", "minus"),
    ("=", "equal"),
    ("`", "grave"),
    ("\\", "backslash"),
];

const MODIFIERS: [(&str, &str); 4] = [("Ctrl", "<Control>"), ("Alt", "<Alt>"), ("Shift", "<Shift>"), ("Super", "<Super>")];

/// Turns keys as people write them ("Ctrl+Alt+F", "super + space") into
/// a GNOME binding ("<Control><Alt>f"). A binding is accepted as it is.
pub fn shortcut_binding(keys: &str) -> Result<String, String> {
    let keys = keys.trim();
    if keys.starts_with('<') && keys.ends_with(|c: char| c != '>') && !keys.contains(['\'', ' ']) {
        return Ok(keys.to_string());
    }
    let mut parts: Vec<&str> = keys.split('+').map(str::trim).collect();
    let key = parts.pop().filter(|k| !k.is_empty()).ok_or("type the keys of the shortcut, such as Ctrl+Alt+F")?;
    let mut held = [false; 4];
    for part in parts {
        let which = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => 0,
            "alt" => 1,
            "shift" => 2,
            "super" | "win" | "windows" | "meta" | "cmd" => 3,
            _ => return Err(format!("\"{part}\" is not Ctrl, Alt, Shift or Super")),
        };
        held[which] = true;
    }
    let lower = key.to_ascii_lowercase();
    let function_key = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=12).contains(n));
    let name = if let Some(n) = function_key {
        format!("F{n}")
    } else if let Some((_, name)) = NAMED_KEYS.iter().find(|(ours, _)| ours.eq_ignore_ascii_case(key)) {
        name.to_string()
    } else if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        lower
    } else {
        return Err(format!("\"{key}\" is not a key this can use: a letter, a digit, F1 to F12, Space..."));
    };
    // A bare letter as a global shortcut would make it impossible to type.
    if !(held[0] || held[1] || held[3]) && function_key.is_none() {
        return Err("a shortcut needs Ctrl, Alt or Super, such as Ctrl+Alt+F".into());
    }
    let mods: String = MODIFIERS.iter().zip(held).filter(|(_, on)| *on).map(|((_, gnome), _)| *gnome).collect();
    Ok(mods + &name)
}

/// A GNOME binding as people write it: "<Control><Alt>f" is "Ctrl+Alt+F".
pub fn shortcut_label(binding: &str) -> String {
    let mut rest = binding.trim();
    let mut out = String::new();
    while let Some((tag, after)) = rest.strip_prefix('<').and_then(|r| r.split_once('>')) {
        let tag = format!("<{tag}>");
        let ours = match tag.as_str() {
            "<Primary>" | "<Ctrl>" => "Ctrl",
            "<Mod1>" => "Alt",
            "<Mod4>" | "<Meta>" => "Super",
            _ => MODIFIERS.iter().find(|(_, gnome)| *gnome == tag).map_or(tag.trim_matches(['<', '>']), |(ours, _)| ours),
        };
        out = out + ours + "+";
        rest = after;
    }
    let key = match NAMED_KEYS.iter().find(|(_, gnome)| *gnome == rest) {
        Some((ours, _)) => ours.to_string(),
        None if rest.len() == 1 => rest.to_ascii_uppercase(),
        None => rest.to_string(),
    };
    out + &key
}

#[cfg(not(windows))]
fn gsettings(args: &[&str]) -> io::Result<String> {
    let out = Command::new("gsettings").args(args).stdin(Stdio::null()).output().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => other("this needs GNOME's gsettings command"),
        _ => e,
    })?;
    if !out.status.success() {
        return Err(other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The desktop's list of custom shortcut paths, e.g. `['/a/', '/b/']`.
#[cfg(not(windows))]
fn custom_shortcuts() -> io::Result<Vec<String>> {
    let list = gsettings(&["get", KEYS_SCHEMA, "custom-keybindings"])?;
    Ok(list.split('\'').skip(1).step_by(2).map(String::from).collect())
}

#[cfg(not(windows))]
pub fn shortcut_enabled() -> bool {
    custom_shortcuts().is_ok_and(|list| list.iter().any(|p| p == KEY_PATH))
}

/// The keys of the shortcut the desktop has for us now, if any.
#[cfg(not(windows))]
pub fn current_shortcut() -> Option<String> {
    let ours = format!("{KEYS_SCHEMA}.custom-keybinding:{KEY_PATH}");
    let binding = gsettings(&["get", &ours, "binding"]).ok()?;
    let binding = binding.trim().trim_matches('\'');
    (shortcut_enabled() && !binding.is_empty()).then(|| shortcut_label(binding))
}

/// The command the desktop runs for the shortcut, as the text `gsettings`
/// takes: this program's path, quoted so that the desktop reads it as
/// one word whatever it contains.
#[cfg(not(windows))]
fn shortcut_command() -> io::Result<String> {
    let exe = launch_path()?;
    let exe = exe.to_str().ok_or_else(|| other("the program path is not valid UTF-8"))?;
    let word = format!("'{}'", exe.replace('\'', "'\\''"));
    Ok(format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// Versions before 0.10 could save the command as "path (deleted)" when
/// the program had been replaced while it ran; the desktop then started
/// it with "(deleted)" as the search text. Puts the right command back.
#[cfg(not(windows))]
pub fn repair_shortcut() {
    let ours = format!("{KEYS_SCHEMA}.custom-keybinding:{KEY_PATH}");
    let Ok(saved) = gsettings(&["get", &ours, "command"]) else { return };
    if saved.trim().trim_matches(['\'', '"']).ends_with(" (deleted)") {
        if let Ok(command) = shortcut_command() {
            let _ = gsettings(&["set", &ours, "command", &command]);
        }
    }
}

/// Adds the shortcut `keys` (as in "Ctrl+Alt+F") that starts this
/// program, or removes ours. Returns the new state.
#[cfg(not(windows))]
pub fn set_shortcut(on: bool, keys: &str) -> io::Result<bool> {
    let binding = if on { shortcut_binding(keys).map_err(other)? } else { String::new() };
    let mut list = custom_shortcuts()?;
    list.retain(|p| p != KEY_PATH);
    let ours = format!("{KEYS_SCHEMA}.custom-keybinding:{KEY_PATH}");
    if on {
        gsettings(&["set", &ours, "name", "Fast search"])?;
        gsettings(&["set", &ours, "command", &shortcut_command()?])?;
        gsettings(&["set", &ours, "binding", &binding])?;
        list.push(KEY_PATH.into());
    } else {
        for key in ["name", "command", "binding"] {
            gsettings(&["reset", &ours, key])?;
        }
    }
    let quoted: Vec<String> = list.iter().map(|p| format!("'{p}'")).collect();
    gsettings(&["set", KEYS_SCHEMA, "custom-keybindings", &format!("[{}]", quoted.join(", "))])?;
    Ok(on)
}

// ---- live-updates service (ffd) ---------------------------------------------

#[cfg(not(windows))]
const INSTALLED_DAEMON: &str = "/usr/local/bin/ffd";

#[cfg(not(windows))]
pub fn daemon_installed() -> bool {
    Path::new("/etc/systemd/system/ffd.service").exists()
}

/// Name of this user's primary group, who will be allowed to read the index.
#[cfg(not(windows))]
fn primary_group() -> io::Result<String> {
    let status = fs::read_to_string("/proc/self/status")?;
    let gid = status
        .lines()
        .find_map(|l| l.strip_prefix("Gid:"))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| other("cannot determine your group"))?;
    let groups = fs::read_to_string("/etc/group")?;
    groups
        .lines()
        .find_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?;
            (f.nth(1)? == gid).then(|| name.to_string())
        })
        .ok_or_else(|| other(format!("group {gid} has no name in /etc/group")))
}

/// Installs or removes the root service, asking for the password through
/// the desktop's authorization dialog (pkexec). Blocks until answered;
/// returns a message for the status bar.
#[cfg(not(windows))]
pub fn set_daemon(install: bool) -> String {
    let run = || -> io::Result<String> {
        // The ffd binary: next to this program, or already installed.
        let beside = std::env::current_exe()?.with_file_name("ffd");
        let installed = PathBuf::from(INSTALLED_DAEMON);
        // A packaged install keeps ffd in /usr/bin, next to this program.
        let ffd = match (install, beside.exists(), installed.exists()) {
            (true, true, _) | (false, true, false) => beside,
            (_, _, true) => installed.clone(),
            _ => return Err(other(format!("ffd not found next to this program or at {INSTALLED_DAEMON}"))),
        };
        // Inside an AppImage the file is on a mount root cannot read:
        // give the installer a copy it can.
        let ffd = if std::env::var_os("APPIMAGE").is_some() && ffd != installed {
            let copy = std::env::temp_dir().join(format!("ffd-{}", std::process::id()));
            fs::copy(&ffd, &copy)?;
            copy
        } else {
            ffd
        };
        let mut cmd = Command::new("pkexec");
        cmd.arg(ffd);
        if install {
            cmd.args(["--install-service", &primary_group()?]);
        } else {
            cmd.arg("--uninstall-service");
        }
        let out = cmd.stdin(Stdio::null()).output().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => other("this needs pkexec (polkit) to ask for the password"),
            _ => e,
        })?;
        match out.status.code() {
            Some(0) if install => Ok("Live updates are on: the ffd service is running".into()),
            Some(0) => Ok("Live updates are off: the ffd service was removed".into()),
            Some(126 | 127) => Err(other("not authorized")),
            _ => Err(other(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed").to_string())),
        }
    };
    run().unwrap_or_else(|e| format!("Live updates not changed: {e}"))
}

// ---- drives -----------------------------------------------------------------

#[cfg(not(windows))]
const DAEMON_CONFIG: &str = "/etc/ffd.conf";

/// A partition as shown on the Drives screen.
pub struct Drive {
    /// Device path, e.g. `/dev/sdb1`.
    pub path: String,
    pub fstype: String,
    pub size: String,
    /// Where it is mounted; empty if it is not.
    pub mount: String,
    /// It can be indexed in its current state: any mounted filesystem, or
    /// an ext2/3/4 or NTFS one read directly while not mounted.
    pub can_index: bool,
    pub index: bool,
    /// Follow changes as they happen (mounted drives only).
    pub live: bool,
}

/// True unless the daemon is set to index only the drives named to it.
pub fn drives_automatic() -> bool {
    !fs::read_to_string(DAEMON_CONFIG).unwrap_or_default().lines().any(|l| l.trim() == "manual")
}

/// The partitions with a filesystem we know, with their current settings
/// from the daemon's configuration.
#[cfg(not(windows))]
pub fn drives() -> Vec<Drive> {
    let listing = Command::new("lsblk").args(["-P", "-p", "-o", "NAME,FSTYPE,MOUNTPOINT,SIZE"]).output();
    let listing = listing.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let config = fs::read_to_string(DAEMON_CONFIG).unwrap_or_default();
    let has = |key: &str, path: &str| config.lines().any(|l| l.split_whitespace().eq([key, path]));
    let mut drives = Vec::new();
    for line in listing.lines() {
        // Each line is KEY="value" pairs.
        let field = |key: &str| -> String {
            let start = line.find(&format!("{key}=\"")).map(|i| i + key.len() + 2);
            start.and_then(|s| line[s..].find('"').map(|e| line[s..s + e].to_string())).unwrap_or_default()
        };
        let (path, fstype, mount) = (field("NAME"), field("FSTYPE"), field("MOUNTPOINT"));
        let known = ["ext2", "ext3", "ext4", "ntfs", "btrfs", "xfs", "f2fs", "vfat", "exfat"].contains(&fstype.as_str());
        if !known || path.is_empty() {
            continue;
        }
        let mounted = !mount.is_empty();
        let readable_unmounted = ["ext2", "ext3", "ext4", "ntfs"].contains(&fstype.as_str());
        drives.push(Drive {
            can_index: mounted || readable_unmounted,
            index: match (mounted, drives_automatic()) {
                (true, true) => !has("skip", &path),
                (true, false) => has("include", &path),
                (false, _) => has("device", &path),
            },
            live: mounted && !has("nolive", &path),
            size: field("SIZE"),
            path,
            fstype,
            mount,
        });
    }
    drives
}

/// Saves the drive settings through the daemon (which restarts itself),
/// asking for the password. Returns a message for the status bar.
/// The `ffd --configure` arguments that express these settings. Each
/// drive is (device path, mounted, index, live).
pub fn configure_args(settings: &[(String, bool, bool, bool)], automatic: bool, excludes: &[String], no_hidden: bool) -> Vec<String> {
    let mut args = vec!["--configure".to_string()];
    if !automatic {
        args.push("--manual".into());
    }
    if no_hidden {
        args.push("--no-hidden".into());
    }
    for folder in excludes {
        args.extend(["--exclude".into(), folder.clone()]);
    }
    // When drives are included automatically we name the ones left out;
    // otherwise the ones wanted. Unmounted drives are always named.
    for (path, mounted, index, live) in settings {
        let flag = match (*mounted, *index) {
            (true, false) if automatic => Some("--skip"),
            (true, true) if !automatic => Some("--include"),
            (false, true) => Some("--device"),
            _ => None,
        };
        args.extend(flag.into_iter().flat_map(|f| [f.to_string(), path.clone()]));
        if *mounted && *index && !*live {
            args.extend(["--nolive".into(), path.clone()]);
        }
    }
    args
}

#[cfg(not(windows))]
pub fn apply_drives(
    settings: Vec<(String, bool, bool, bool)>,
    automatic: bool,
    excludes: Vec<String>,
    no_hidden: bool,
) -> String {
    let run = || -> io::Result<String> {
        let installed = PathBuf::from(INSTALLED_DAEMON);
        let ffd = if installed.exists() { installed } else { std::env::current_exe()?.with_file_name("ffd") };
        let mut cmd = Command::new("pkexec");
        cmd.arg(ffd).args(configure_args(&settings, automatic, &excludes, no_hidden));
        let out = cmd.stdin(Stdio::null()).output().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => other("this needs pkexec (polkit) to ask for the password"),
            _ => e,
        })?;
        match out.status.code() {
            Some(0) => Ok("Index settings saved; the index is being rebuilt".into()),
            Some(126 | 127) => Err(other("not authorized")),
            _ => Err(other(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed").to_string())),
        }
    };
    run().unwrap_or_else(|e| format!("Index settings not changed: {e}"))
}

// ---- Windows: Start menu entry, hot key, live-updates service, drives --------

#[cfg(windows)]
use crate::winsys;

#[cfg(windows)]
fn launch_path() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// True if Windows is set to dark mode for applications.
#[cfg(windows)]
pub fn desktop_prefers_dark() -> bool {
    let key = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    winsys::reg_dword(winsys::HKEY_CURRENT_USER, key, "AppsUseLightTheme") == Some(0)
}

/// The Start menu shortcut, which also carries the global hot key.
#[cfg(windows)]
fn start_menu_link() -> io::Result<PathBuf> {
    let appdata = ff_core::os::config_dir().ok_or_else(|| other("APPDATA is not set"))?;
    Ok(appdata.join(r"Microsoft\Windows\Start Menu\Programs\Fast search.lnk"))
}

/// Where the chosen hot key is remembered (a .lnk is not easily read back).
#[cfg(windows)]
fn shortcut_state() -> io::Result<PathBuf> {
    Ok(ff_core::os::config_dir().ok_or_else(|| other("APPDATA is not set"))?.join("ff").join("shortcut.txt"))
}
/// Where Windows keeps the programs to start at sign-in, for this user.
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Our entry there.
#[cfg(windows)]
pub const RUN_VALUE: &str = "Fast search";

/// True if Fast search starts at sign-in (hidden, in the notification area).
#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    winsys::reg_string_exists(winsys::HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE)
}

/// Makes Fast search start at sign-in, or not. It starts this program,
/// wherever it is, hidden.
#[cfg(windows)]
pub fn set_autostart(on: bool) -> io::Result<()> {
    if !on {
        return winsys::reg_delete_value(winsys::HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
    }
    let exe = ff_core::os::strip_verbatim(std::env::current_exe()?);
    winsys::reg_set_string(winsys::HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, &format!("\"{}\" --hidden", exe.display()))
}


#[cfg(windows)]
pub fn launcher_installed() -> bool {
    start_menu_link().is_ok_and(|l| l.exists())
}

/// Adds "Fast search" to the Start menu (keeping the hot key, if any).
#[cfg(windows)]
pub fn install_launcher() -> io::Result<PathBuf> {
    let link = start_menu_link()?;
    let hotkey = current_shortcut().map(|k| windows_hotkey(&k)).transpose().map_err(other)?.unwrap_or_default();
    winsys::write_shortcut(&link, &launch_path()?, &hotkey)?;
    Ok(link)
}

#[cfg(windows)]
pub fn remove_launcher() -> io::Result<()> {
    let _ = shortcut_state().and_then(fs::remove_file);
    match fs::remove_file(start_menu_link()?) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The keys used until the user chooses others. Windows hot keys need
/// Ctrl and Alt together.
#[cfg(windows)]
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Alt+F";

/// "Ctrl+Alt+F" as a shell link hot key ("CTRL+ALT+F").
#[cfg(windows)]
fn windows_hotkey(label: &str) -> Result<String, String> {
    let parts: Vec<&str> = label.split('+').collect();
    let (key, mods) = parts.split_last().ok_or("no key given")?;
    if mods.contains(&"Super") {
        return Err("Windows shortcuts cannot use the Windows key; use Ctrl+Alt, such as Ctrl+Alt+F".into());
    }
    if !(mods.contains(&"Ctrl") && mods.contains(&"Alt")) && !key.starts_with('F') {
        return Err("on Windows a shortcut needs Ctrl+Alt, such as Ctrl+Alt+F".into());
    }
    let mut out: Vec<String> = mods.iter().map(|m| m.to_uppercase()).collect();
    out.push(key.to_uppercase());
    Ok(out.join("+"))
}

#[cfg(windows)]
pub fn shortcut_enabled() -> bool {
    launcher_installed() && shortcut_state().is_ok_and(|s| s.exists())
}

#[cfg(windows)]
pub fn current_shortcut() -> Option<String> {
    let text = fs::read_to_string(shortcut_state().ok()?).ok()?;
    let keys = text.trim();
    (!keys.is_empty()).then(|| keys.to_string())
}

/// Nothing to mend on Windows.
#[cfg(windows)]
pub fn repair_shortcut() {}

/// Sets the hot key `keys` (as in "Ctrl+Alt+F") on the Start menu
/// shortcut, creating it if needed, or takes the hot key away.
#[cfg(windows)]
pub fn set_shortcut(on: bool, keys: &str) -> io::Result<bool> {
    let link = start_menu_link()?;
    let state = shortcut_state()?;
    if on {
        let label = shortcut_label(&shortcut_binding(keys).map_err(other)?);
        let hotkey = windows_hotkey(&label).map_err(other)?;
        winsys::write_shortcut(&link, &launch_path()?, &hotkey)?;
        fs::create_dir_all(state.parent().unwrap_or(Path::new(".")))?;
        fs::write(&state, label)?;
    } else {
        if link.exists() {
            winsys::write_shortcut(&link, &launch_path()?, "")?;
        }
        let _ = fs::remove_file(&state);
    }
    Ok(on)
}

#[cfg(windows)]
const DAEMON_CONFIG: &str = r"C:\ProgramData\ff\ffd.conf";

/// The service program, next to this one.
#[cfg(windows)]
fn ffd_path() -> io::Result<PathBuf> {
    let ffd = std::env::current_exe()?.with_file_name("ffd.exe");
    if ffd.exists() { Ok(ffd) } else { Err(other(format!("{} not found", ffd.display()))) }
}

#[cfg(windows)]
pub fn daemon_installed() -> bool {
    winsys::reg_key_exists(winsys::HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Services\ffd")
}

/// Installs or removes the `ffd` service; Windows asks for administrator
/// rights first. Blocks until answered; returns a status bar message.
#[cfg(windows)]
pub fn set_daemon(install: bool) -> String {
    let run = || -> io::Result<String> {
        let arg = if install { "--install-service" } else { "--uninstall-service" };
        match winsys::run_elevated(&ffd_path()?, &[arg.to_string()])? {
            0 if install => Ok("Live updates are on: the ffd service is running".into()),
            0 => Ok("Live updates are off: the ffd service was removed".into()),
            code => Err(other(format!("ffd failed (exit code {code})"))),
        }
    };
    run().unwrap_or_else(|e| format!("Live updates not changed: {e}"))
}

/// The local fixed drives, with the service's settings for them.
#[cfg(windows)]
pub fn drives() -> Vec<Drive> {
    let config = fs::read_to_string(DAEMON_CONFIG).unwrap_or_default();
    let has = |key: &str, path: &str| config.lines().any(|l| l.split_whitespace().eq([key, path]));
    let automatic = drives_automatic();
    ff_core::win::fixed_drives()
        .into_iter()
        .map(|root| {
            let mount = root.to_string_lossy().into_owned();
            let path = mount.trim_end_matches(std::path::MAIN_SEPARATOR).to_string();
            let (fstype, bytes) = winsys::volume_info(&root);
            Drive {
                can_index: true,
                index: if automatic { !has("skip", &path) } else { has("include", &path) },
                live: !has("nolive", &path),
                size: ff_core::sizes::human_size(bytes),
                fstype: fstype.to_lowercase(),
                path,
                mount,
            }
        })
        .collect()
}

/// Saves the drive settings through the service (which restarts itself),
/// asking for administrator rights. Returns a status bar message.
#[cfg(windows)]
pub fn apply_drives(
    settings: Vec<(String, bool, bool, bool)>,
    automatic: bool,
    excludes: Vec<String>,
    no_hidden: bool,
) -> String {
    let run = || -> io::Result<String> {
        match winsys::run_elevated(&ffd_path()?, &configure_args(&settings, automatic, &excludes, no_hidden))? {
            0 => Ok("Index settings saved; the index is being rebuilt".into()),
            code => Err(other(format!("ffd failed (exit code {code})"))),
        }
    };
    run().unwrap_or_else(|e| format!("Index settings not changed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_as_written_and_as_gnome_wants_them() {
        for (written, binding, label) in [
            ("Alt+E", "<Alt>e", "Alt+E"),
            ("ctrl + alt + f", "<Control><Alt>f", "Ctrl+Alt+F"),
            ("Super+Space", "<Super>space", "Super+Space"),
            ("win+shift+/", "<Shift><Super>slash", "Shift+Super+/"),
            ("F9", "F9", "F9"),
            ("<Primary><Alt>k", "<Primary><Alt>k", "Ctrl+Alt+K"),
        ] {
            assert_eq!(shortcut_binding(written).as_deref(), Ok(binding), "{written}");
            assert_eq!(shortcut_label(binding), label);
        }
        for bad in ["", "e", "Shift+E", "Ctrl+", "Hyper+E", "Ctrl+Banana"] {
            assert!(shortcut_binding(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn drive_settings_become_daemon_arguments() {
        let drive = |path: &str, mounted, index, live| (path.to_string(), mounted, index, live);
        let drives = [
            drive("/dev/a1", true, true, true),    // mounted, indexed, live: nothing to say
            drive("/dev/b1", true, false, false),  // mounted, switched off
            drive("/dev/c1", true, true, false),   // mounted, indexed, not live
            drive("/dev/d1", false, true, false),  // not mounted, read directly
            drive("/dev/e1", false, false, false), // not mounted, left alone
        ];
        let args = configure_args(&drives, true, &[], false);
        assert_eq!(args, ["--configure", "--skip", "/dev/b1", "--nolive", "/dev/c1", "--device", "/dev/d1"]);

        // Manual mode names what is wanted instead of what is not.
        let args = configure_args(&drives, false, &["/home/me/My Videos".into()], true);
        assert_eq!(
            args,
            [
                "--configure", "--manual", "--no-hidden", "--exclude", "/home/me/My Videos", "--include", "/dev/a1",
                "--include", "/dev/c1", "--nolive", "/dev/c1", "--device", "/dev/d1"
            ]
        );
    }
}
