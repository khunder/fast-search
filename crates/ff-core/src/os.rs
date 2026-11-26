//! The few things that differ between Unix and Windows, in one place:
//! paths as bytes, the path separator, file metadata, default roots.
//!
//! The index stores every name as bytes. On Unix those are the raw bytes
//! the kernel gave us. On Windows they are the standard library's
//! "encoded bytes" of an `OsStr` (WTF-8: UTF-8, plus unpaired UTF-16
//! surrogates for the rare name that is not valid Unicode), so every
//! Windows name round-trips exactly.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The byte that separates path components in the index and in output.
#[cfg(unix)]
pub const SEP: u8 = b'/';
#[cfg(windows)]
pub const SEP: u8 = b'\\';

/// The separator as a string.
#[cfg(unix)]
pub const SEP_STR: &str = "/";
#[cfg(windows)]
pub const SEP_STR: &str = "\\";

/// True for a byte that separates path components. Windows also accepts
/// `/`, so a search term may use either.
#[inline]
pub fn is_sep(b: u8) -> bool {
    b == b'/' || (cfg!(windows) && b == b'\\')
}

/// The bytes of an OS string, as stored in the index.
#[inline]
pub fn bytes(s: &OsStr) -> &[u8] {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::as_bytes(s)
    }
    #[cfg(windows)]
    {
        s.as_encoded_bytes()
    }
}

/// The bytes of a path, as stored in the index.
#[inline]
pub fn path_bytes(p: &Path) -> &[u8] {
    bytes(p.as_os_str())
}

/// Bytes from the index (or anywhere) back to an OS string. On Windows,
/// bytes that are not WTF-8 (a damaged file, a C caller) are decoded
/// lossily rather than trusted.
pub fn os_string(b: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        <OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b).to_owned()
    }
    #[cfg(windows)]
    {
        match std::str::from_utf8(b) {
            Ok(s) => OsString::from(s),
            Err(_) => {
                use std::os::windows::ffi::OsStringExt;
                OsString::from_wide(&wtf8_to_wide(b))
            }
        }
    }
}

/// Owned variant of [`os_string`].
pub fn os_string_from_vec(b: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStringExt::from_vec(b)
    }
    #[cfg(windows)]
    {
        match String::from_utf8(b) {
            Ok(s) => OsString::from(s),
            Err(e) => os_string(e.as_bytes()),
        }
    }
}

/// Bytes back to a path.
pub fn path_from_bytes(b: &[u8]) -> PathBuf {
    os_string(b).into()
}

/// Decodes WTF-8 (UTF-8 that may hold surrogates) to UTF-16; anything
/// malformed becomes U+FFFD.
#[cfg(windows)]
fn wtf8_to_wide(b: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let (len, init) = match c {
            0x00..=0x7F => (1, c as u32),
            0xC2..=0xDF => (2, (c & 0x1F) as u32),
            0xE0..=0xEF => (3, (c & 0x0F) as u32),
            0xF0..=0xF4 => (4, (c & 0x07) as u32),
            _ => (0, 0),
        };
        let cont = |k: usize| b.get(i + k).filter(|&&x| x & 0xC0 == 0x80).map(|&x| (x & 0x3F) as u32);
        let mut cp = Some(init);
        for k in 1..len {
            cp = cp.and_then(|v| cont(k).map(|x| (v << 6) | x));
        }
        match cp.filter(|_| len > 0) {
            Some(cp) if cp >= 0x10000 => {
                let v = cp - 0x10000;
                out.push(0xD800 | (v >> 10) as u16);
                out.push(0xDC00 | (v & 0x3FF) as u16);
                i += len;
            }
            Some(cp) => {
                out.push(cp as u16);
                i += len;
            }
            None => {
                out.push(0xFFFD);
                i += 1;
            }
        }
    }
    out
}

/// A path as a NUL-terminated UTF-16 string, for Windows API calls.
#[cfg(windows)]
pub fn wide(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

/// Size in bytes.
#[inline]
pub fn size(md: &fs::Metadata) -> u64 {
    md.len()
}

/// Modification time in seconds since the Unix epoch (negative before it).
pub fn mtime(md: &fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::mtime(md)
    }
    #[cfg(not(unix))]
    {
        system_secs(md.modified())
    }
}

/// True for an entry that Windows marks hidden (always false on Unix,
/// where only a leading dot hides a name).
#[inline]
pub fn hidden_attribute(md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        std::os::windows::fs::MetadataExt::file_attributes(md) & FILE_ATTRIBUTE_HIDDEN != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

/// Last access time in seconds since the Unix epoch.
pub fn atime(md: &fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::atime(md)
    }
    #[cfg(not(unix))]
    {
        system_secs(md.accessed())
    }
}

/// Makes the calling thread a background one: it runs when nothing else
/// wants the processor, and (on Windows) its disk reads wait for others'.
/// For work nobody waits for, so a small laptop stays responsive.
pub fn background_thread() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
        // SAFETY: the pseudo-handle of this thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    }
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn gettid() -> i32;
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        // On Linux a thread's nice value is its own: PRIO_PROCESS with
        // the thread id sets just this one.
        // SAFETY: plain system calls.
        unsafe { setpriority(0, gettid() as u32, 19) };
    }
}

/// The modification time of `path` (as [`mtime`] gives it), cheaply: on
/// Windows without opening it (`GetFileAttributesExW`), which matters when
/// checking every folder of a drive. None if it is not there.
pub fn quick_mtime(path: &Path) -> Option<i64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFileAttributesExW(name: *const u16, level: i32, info: *mut u8) -> i32;
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // WIN32_FILE_ATTRIBUTE_DATA: attributes, then three FILETIMEs (pairs
        // of u32, so read through bytes), the last write at offset 20.
        let mut raw = [0u8; 36];
        // SAFETY: a NUL-terminated name and a buffer of the struct's size.
        if unsafe { GetFileAttributesExW(name.as_ptr(), 0, raw.as_mut_ptr()) } == 0 {
            return None;
        }
        let ticks = u64::from_le_bytes(raw[20..28].try_into().ok()?) as i64;
        // 100 ns units since 1601, to seconds since 1970 (floored, as
        // `mtime` does).
        Some(ticks.div_euclid(10_000_000) - 11_644_473_600)
    }
    #[cfg(not(windows))]
    {
        fs::symlink_metadata(path).ok().map(|m| mtime(&m))
    }
}

#[allow(dead_code)]
fn system_secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
        },
        Err(_) => 0,
    }
}

/// `fs::canonicalize`, but on Windows without the `\\?\` prefix the
/// standard library adds (`\\?\C:\x` -> `C:\x`, `\\?\UNC\s\x` -> `\\s\x`),
/// so paths read and print the way people write them.
pub fn canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    let full = fs::canonicalize(p)?;
    Ok(strip_verbatim(full))
}

/// Removes a `\\?\` prefix where the path stays valid without it.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_os_str().as_encoded_bytes();
        if let Some(rest) = s.strip_prefix(br"\\?\UNC\") {
            let mut out = br"\\".to_vec();
            out.extend_from_slice(rest);
            return path_from_bytes(&out);
        }
        if let Some(rest) = s.strip_prefix(br"\\?\") {
            if rest.len() >= 2 && rest[0].is_ascii_alphabetic() && rest[1] == b':' {
                return path_from_bytes(rest);
            }
        }
    }
    p
}

/// What to index when nothing is named: `/` on Unix; every local fixed
/// drive on Windows (`C:\`, `D:\`, ...).
pub fn default_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let drives = crate::win::fixed_drives();
        if !drives.is_empty() {
            return drives;
        }
        vec![PathBuf::from("C:\\")]
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

/// Folders skipped by default: kernel pseudo-filesystems on Unix; on
/// Windows, the system's own bookkeeping folders on each fixed drive.
pub fn default_excludes() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for drive in default_roots() {
            for name in ["System Volume Information", "$Recycle.Bin", "$WinREAgent", "Config.Msi"] {
                out.push(drive.join(name));
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        ["/proc", "/sys", "/dev", "/run", "/.snapshots"].map(PathBuf::from).into()
    }
}

/// The user's home folder: `$HOME`, or on Windows `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    #[cfg(windows)]
    {
        var("USERPROFILE").or_else(|| var("HOME")).map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        var("HOME").map(PathBuf::from)
    }
}

/// Where settings go: `$XDG_CONFIG_HOME` or `~/.config` on Unix,
/// `%APPDATA%` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    #[cfg(windows)]
    {
        var("APPDATA").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join("AppData").join("Roaming")))
    }
    #[cfg(not(windows))]
    {
        var("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".config")))
    }
}

/// Where caches go: `$XDG_CACHE_HOME` or `~/.cache` on Unix,
/// `%LOCALAPPDATA%` on Windows.
pub fn cache_dir() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    #[cfg(windows)]
    {
        var("LOCALAPPDATA").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join("AppData").join("Local")))
    }
    #[cfg(not(windows))]
    {
        var("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".cache")))
    }
}

/// Where data files go: `$XDG_DATA_HOME` or `~/.local/share` on Unix,
/// `%LOCALAPPDATA%` on Windows.
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        cache_dir()
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".local/share")))
    }
}

/// The capacity and free space, in bytes, of the filesystem `path` is on
/// (free: all unused space, so that used = capacity - free). None where
/// unknown.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        crate::win::disk_space(path)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        // struct statvfs on 64-bit Linux (glibc and musl): eleven 8-byte
        // fields, then spare room; f_frsize, f_blocks and f_bfree are the
        // 2nd, 3rd and 4th.
        unsafe extern "C" {
            fn statvfs(path: *const std::ffi::c_char, buf: *mut u64) -> i32;
        }
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf = [0u64; 16];
        // SAFETY: a NUL-terminated path and a buffer larger than the struct.
        if unsafe { statvfs(c.as_ptr(), buf.as_mut_ptr()) } != 0 {
            return None;
        }
        let (frsize, blocks, free) = (buf[1], buf[2], buf[3]);
        (blocks > 0).then(|| (blocks * frsize, free * frsize))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = path;
        None
    }
}

/// Reads exactly `buf.len()` bytes at `offset`. On Windows the read is
/// widened to whole 4 KiB blocks, which raw volumes (`\\.\D:`) require.
pub fn read_exact_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        const BLOCK: u64 = 4096;
        let fill = |buf: &mut [u8], start: u64| -> std::io::Result<usize> {
            let mut done = 0;
            while done < buf.len() {
                match file.seek_read(&mut buf[done..], start + done as u64) {
                    Ok(0) => break,
                    Ok(n) => done += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(done)
        };
        // Whole blocks (the big reads of a master file table) go straight
        // into the caller's buffer.
        if offset % BLOCK == 0 && buf.len() as u64 % BLOCK == 0 {
            let len = buf.len();
            return if fill(buf, offset)? == len { Ok(()) } else { Err(std::io::ErrorKind::UnexpectedEof.into()) };
        }
        let start = offset / BLOCK * BLOCK;
        let end = (offset + buf.len() as u64).div_ceil(BLOCK) * BLOCK;
        let mut tmp = vec![0u8; (end - start) as usize];
        let done = fill(&mut tmp, start)?;
        let skip = (offset - start) as usize;
        if done < skip + buf.len() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip() {
        for s in ["plain", "Ünïcode ☃.txt", "a b", ""] {
            let os = OsStr::new(s);
            assert_eq!(os_string(bytes(os)), os);
        }
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_surrogates_round_trip() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let odd = OsString::from_wide(&[b'a' as u16, 0xD800, b'b' as u16]);
        let back = os_string(bytes(&odd));
        assert_eq!(back.encode_wide().collect::<Vec<_>>(), [b'a' as u16, 0xD800, b'b' as u16]);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_dropped() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\Users")), PathBuf::from(r"C:\Users"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\Volume{x}\")), PathBuf::from(r"\\?\Volume{x}\"));
    }
}
