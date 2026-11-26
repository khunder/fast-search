//! Desktop actions on a found file: open it, show it in the file manager,
//! read its properties. Shared by the CLI and the GUI.
//!
//! On Linux these go through the freedesktop tools (`xdg-open`, `gio`,
//! D-Bus); on Windows through the shell (`ShellExecute`, Explorer, the
//! Recycle Bin).

use std::fs;
use std::io;
use std::path::Path;
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

#[cfg(not(windows))]
fn succeeds(cmd: &mut Command) -> io::Result<bool> {
    let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    Ok(status.success())
}

/// Opens `path` with its default application (via `xdg-open`; on
/// Windows, as a double click in Explorer would).
#[cfg(not(windows))]
pub fn open(path: &Path) -> io::Result<()> {
    match succeeds(Command::new("xdg-open").arg(path)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(io::Error::other(format!("xdg-open could not open {}", path.display()))),
        Err(e) => Err(io::Error::new(e.kind(), format!("cannot run xdg-open: {e}"))),
    }
}

#[cfg(windows)]
pub fn open(path: &Path) -> io::Result<()> {
    crate::win::shell_execute("open", path)
}

/// Opens `path` in the desktop's default text editor, whatever its type
/// (useful for scripts, HTML, JSON and other files whose normal "open"
/// runs them or shows them in another program). Falls back to a plain
/// open when the editor cannot be determined.
#[cfg(not(windows))]
pub fn open_in_editor(path: &Path) -> io::Result<()> {
    let editor = Command::new("xdg-mime").args(["query", "default", "text/plain"]).output();
    let editor = editor.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    // gtk-launch starts a desktop entry by id with the file as argument.
    if !editor.is_empty() && succeeds(Command::new("gtk-launch").arg(&editor).arg(path)).unwrap_or(false) {
        return Ok(());
    }
    open(path)
}

/// On Windows: the file type's "edit" verb if it has one, else Notepad.
#[cfg(windows)]
pub fn open_in_editor(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return open(path);
    }
    if crate::win::shell_execute("edit", path).is_ok() {
        return Ok(());
    }
    std::process::Command::new("notepad.exe").arg(path).spawn().map(|_| ())
}

/// Moves `path` to the desktop's trash (via `gio trash`; on Windows, the
/// Recycle Bin), from where it can be restored.
#[cfg(not(windows))]
pub fn trash(path: &Path) -> io::Result<()> {
    match succeeds(Command::new("gio").arg("trash").arg("--").arg(path)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(io::Error::other(format!("could not move {} to the trash", path.display()))),
        Err(e) => Err(io::Error::new(e.kind(), format!("cannot run gio: {e}"))),
    }
}

#[cfg(windows)]
pub fn trash(path: &Path) -> io::Result<()> {
    crate::win::recycle(path)
}

/// Deletes `path` for good: a file or link, or a folder with everything
/// in it. A link is removed itself, never what it points to.
pub fn delete(path: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if md.file_type().is_dir() {
        return fs::remove_dir_all(path);
    }
    // A link to a folder (or a junction) is itself a folder to Windows.
    #[cfg(windows)]
    if md.file_type().is_symlink() && std::os::windows::fs::MetadataExt::file_attributes(&md) & 0x10 != 0 {
        return fs::remove_dir(path);
    }
    fs::remove_file(path)
}

/// `path` as a `file://` URI.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let bytes = crate::os::path_bytes(path);
    // `C:\x` becomes `file:///C:/x`.
    if cfg!(windows) && bytes.get(1) == Some(&b':') {
        uri.push('/');
    }
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) || (cfg!(windows) && b == b':') {
            uri.push(b as char);
        } else if cfg!(windows) && b == b'\\' {
            uri.push('/');
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Shows `path` selected in the file manager. Falls back to opening its
/// folder when no file manager answers on D-Bus.
#[cfg(not(windows))]
pub fn reveal(path: &Path) -> io::Result<()> {
    let uri = file_uri(path);
    let shown = succeeds(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest=org.freedesktop.FileManager1",
        "--object-path=/org/freedesktop/FileManager1",
        "--method=org.freedesktop.FileManager1.ShowItems",
        &format!("['{uri}']"),
        "''",
    ]));
    if shown.unwrap_or(false) {
        return Ok(());
    }
    open(path.parent().unwrap_or(path))
}

/// On Windows: an Explorer window with the item selected.
#[cfg(windows)]
pub fn reveal(path: &Path) -> io::Result<()> {
    crate::win::reveal(path)
}

/// Reads the current properties of `path` from disk as label/value pairs.
pub fn properties(path: &Path) -> io::Result<Vec<(&'static str, String)>> {
    let md = fs::symlink_metadata(path)?;
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        "Folder"
    } else if ft.is_symlink() {
        "Symbolic link"
    } else {
        special_kind(&ft).unwrap_or("File")
    };
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
    let mut out = vec![("Name", name), ("Type", kind.to_string())];
    if let Some(parent) = path.parent() {
        out.push(("Location", parent.display().to_string()));
    }
    if ft.is_symlink() {
        if let Ok(target) = fs::read_link(path) {
            out.push(("Target", crate::os::strip_verbatim(target).display().to_string()));
        }
    }
    if ft.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            out.push(("Contains", format!("{} items", crate::group_digits(rd.count() as u64))));
        }
    } else {
        out.push(("Size", size_text(md.len())));
    }
    out.push(("Modified", time_text(crate::os::mtime(&md))));
    out.push(("Accessed", time_text(crate::os::atime(&md))));
    if let Some(created) = md.created().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        out.push(("Created", time_text(created.as_secs() as i64)));
    }
    owner_and_mode(&md, &mut out);
    Ok(out)
}

#[cfg(unix)]
fn special_kind(ft: &fs::FileType) -> Option<&'static str> {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_socket() {
        Some("Socket")
    } else if ft.is_fifo() {
        Some("Named pipe")
    } else if ft.is_block_device() || ft.is_char_device() {
        Some("Device")
    } else {
        None
    }
}

#[cfg(not(unix))]
fn special_kind(_: &fs::FileType) -> Option<&'static str> {
    None
}

#[cfg(unix)]
fn owner_and_mode(md: &fs::Metadata, out: &mut Vec<(&'static str, String)>) {
    use std::os::unix::fs::MetadataExt;
    let mode = md.mode();
    let bits: String = (0..9).map(|i| if mode >> (8 - i) & 1 == 1 { b"rwx"[i % 3] as char } else { '-' }).collect();
    out.push(("Permissions", format!("{bits} ({:03o})", mode & 0o7777)));
    let owner = id_name("/etc/passwd", md.uid());
    let group = id_name("/etc/group", md.gid());
    out.push(("Owner", format!("{owner} : {group}")));
}

#[cfg(windows)]
fn owner_and_mode(md: &fs::Metadata, out: &mut Vec<(&'static str, String)>) {
    let attrs = std::os::windows::fs::MetadataExt::file_attributes(md);
    out.push(("Attributes", crate::win::attribute_names(attrs)));
}

fn size_text(bytes: u64) -> String {
    let exact = format!("{} bytes", crate::group_digits(bytes));
    let mut value = bytes as f64;
    let mut unit = "";
    for u in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    if unit.is_empty() { exact } else { format!("{value:.1} {unit} ({exact})") }
}

fn time_text(secs: i64) -> String {
    let (y, mo, d, h, mi) = crate::local_civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// Looks up a numeric id in /etc/passwd or /etc/group (`name:x:id:...`).
#[cfg(unix)]
fn id_name(file: &str, id: u32) -> String {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .find_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?;
            (f.nth(1)?.parse::<u32>().ok()? == id).then(|| name.to_string())
        })
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_removes_files_folders_and_links_but_not_link_targets() {
        let root = std::env::temp_dir().join(format!("ff_delete_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("keep/inner")).unwrap();
        fs::write(root.join("keep/inner/data.txt"), "x").unwrap();
        fs::create_dir_all(root.join("gone/deep")).unwrap();
        fs::write(root.join("gone/deep/a.txt"), "x").unwrap();
        fs::write(root.join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("keep"), root.join("link-to-keep")).unwrap();
        // A junction: what Windows lets anyone make without privileges.
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("link-to-keep"))
                .arg(root.join("keep"))
                .output()
                .unwrap()
                .status
                .success()
        );

        delete(&root.join("file.txt")).unwrap();
        assert!(!root.join("file.txt").exists());

        delete(&root.join("gone")).unwrap();
        assert!(!root.join("gone").exists());

        // Deleting a link to a folder removes the link only.
        delete(&root.join("link-to-keep")).unwrap();
        assert!(fs::symlink_metadata(root.join("link-to-keep")).is_err());
        assert!(root.join("keep/inner/data.txt").exists());

        assert!(delete(&root.join("never-existed")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_uris_escape_special_characters() {
        assert_eq!(file_uri(Path::new("/home/me/a b#c.txt")), "file:///home/me/a%20b%23c.txt");
        assert_eq!(file_uri(Path::new("/plain/path-1_2.3~")), "file:///plain/path-1_2.3~");
    }

    #[cfg(windows)]
    #[test]
    fn file_uris_on_windows() {
        assert_eq!(file_uri(Path::new(r"C:\Users\me\a b#c.txt")), "file:///C:/Users/me/a%20b%23c.txt");
    }
}
