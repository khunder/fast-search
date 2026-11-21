//! Updating from GitHub releases. The latest release is found through
//! GitHub's "latest release" redirect; on Windows its installer is
//! downloaded and run, which replaces the programs in place.
//!
//! Downloads use the `curl` program that ships with Windows 10 and later
//! (and with nearly every Linux system), so no TLS library is needed here.

use std::io;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

/// The repository releases are published from.
pub const REPO: &str = "khunder/fast-search";

/// The Windows installer's file name in every release.
pub const INSTALLER: &str = "fast-search-windows-x86_64-setup.exe";

/// The version of these programs. Release builds take it from the tag
/// being released (`FF_VERSION`); other builds from Cargo.toml.
pub const VERSION: &str = match option_env!("FF_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The newest release.
pub struct Release {
    /// The tag, as published ("0.9.9" or "v0.9.9").
    pub tag: String,
}

impl Release {
    /// The version without a leading "v".
    pub fn version(&self) -> &str {
        self.tag.strip_prefix('v').unwrap_or(&self.tag)
    }

    /// True if this release is newer than the running programs.
    pub fn is_newer(&self) -> bool {
        newer(self.version(), VERSION)
    }

    pub fn page(&self) -> String {
        format!("https://github.com/{REPO}/releases/tag/{}", self.tag)
    }
}

/// The curl that comes with the system: on Windows the one in System32,
/// not whatever a `curl` on PATH may be.
fn curl() -> Command {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let mut cmd = Command::new(PathBuf::from(root).join("System32").join("curl.exe"));
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        Command::new("curl")
    }
}

fn run(cmd: &mut Command, what: &str) -> io::Result<String> {
    let out = cmd.stdin(std::process::Stdio::null()).output().map_err(|e| io::Error::new(e.kind(), format!("cannot run curl: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(io::Error::other(format!("{what} failed: {err}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Asks GitHub for the newest release.
pub fn latest() -> io::Result<Release> {
    // ".../releases/latest" redirects to ".../releases/tag/<tag>".
    let url = format!("https://github.com/{REPO}/releases/latest");
    let target = run(curl().args(["-sS", "--max-time", "20", "-o", null_device(), "-w", "%{redirect_url}", &url]), "checking for updates")?;
    let tag = target.trim().rsplit_once("/releases/tag/").map(|(_, t)| t.to_string()).filter(|t| !t.is_empty());
    tag.map(|tag| Release { tag }).ok_or_else(|| io::Error::other("no release is published yet"))
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// True if version `a` is newer than `b`: numbers compared part by part,
/// so 0.10.0 is newer than 0.9.9. A suffix ("-beta") is ignored.
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The folder of the running program, if it was put there by the
/// installer (which leaves its uninstaller beside the programs).
#[cfg(windows)]
pub fn installed_folder() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("unins000.exe").exists().then_some(dir)
}

/// Downloads the release's installer into the temporary folder.
#[cfg(windows)]
pub fn download(release: &Release) -> io::Result<PathBuf> {
    let url = format!("https://github.com/{REPO}/releases/download/{}/{INSTALLER}", release.tag);
    let file = std::env::temp_dir().join(format!("fast-search-setup-{}.exe", release.version()));
    run(curl().args(["-fsSL", "--max-time", "600", "-o"]).arg(&file).arg(&url), "downloading the installer")
        .map_err(|e| io::Error::other(format!("{e} (release {} may have no Windows installer: {})", release.tag, release.page())))?;
    Ok(file)
}

/// Starts the installer to update in place, and returns at once: it
/// closes the running programs and replaces them, then opens the window
/// again if `reopen_window`. It keeps the folder, the choices made at
/// install time and the install mode (asking for administrator rights if
/// it was for all users).
#[cfg(windows)]
pub fn install(installer: &std::path::Path, reopen_window: bool) -> io::Result<()> {
    let mut cmd = Command::new(installer);
    cmd.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"]);
    // Name the folder and the mode of this install: Windows may run any
    // "setup" program elevated, which would otherwise install for all
    // users, in Program Files, next to a copy installed for one user.
    if let Some(dir) = installed_folder() {
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let for_all = program_files.is_some_and(|p| dir.starts_with(p));
        cmd.arg(if for_all { "/ALLUSERS" } else { "/CURRENTUSER" });
        let mut arg = std::ffi::OsString::from("/DIR=");
        arg.push(dir.as_os_str());
        cmd.arg(arg);
    }
    if reopen_window {
        cmd.arg("/STARTGUI=1");
    }
    cmd.spawn().map(drop)
}

/// Checks for a newer release and, if there is one, downloads and starts
/// its installer. Returns what happened, as a sentence.
#[cfg(windows)]
pub fn update(reopen_window: bool, progress: impl Fn(&str)) -> io::Result<String> {
    let release = latest()?;
    if !release.is_newer() {
        return Ok(format!("Fast search is up to date ({VERSION})"));
    }
    if installed_folder().is_none() {
        return Ok(format!(
            "Fast search {} is available, but these programs were not installed by its installer: download it from {}",
            release.version(),
            release.page()
        ));
    }
    progress(&format!("Downloading Fast search {}...", release.version()));
    let installer = download(&release)?;
    install(&installer, reopen_window)?;
    Ok(format!("Installing Fast search {}...", release.version()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(newer("0.9.10", "0.9.9"));
        assert!(!newer("0.9.9", "0.9.9"));
        assert!(!newer("0.9.9-beta", "0.9.9"));
        assert!(!newer("0.9", "0.9.0"));
        assert!(!newer("0.8.0", "0.9.0"));
    }
}
