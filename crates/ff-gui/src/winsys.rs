//! Windows system services for the window program, declared by hand:
//! the console, message boxes, the registry and running a program with
//! administrator rights.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{OsStr, c_void};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

type HANDLE = *mut c_void;
type HKEY = isize;

pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize;
pub const HKEY_LOCAL_MACHINE: HKEY = 0x8000_0002u32 as i32 as isize;

#[repr(C)]
struct SHELLEXECUTEINFOW {
    cbSize: u32,
    fMask: u32,
    hwnd: HANDLE,
    lpVerb: *const u16,
    lpFile: *const u16,
    lpParameters: *const u16,
    lpDirectory: *const u16,
    nShow: i32,
    hInstApp: HANDLE,
    lpIDList: *mut c_void,
    lpClass: *const u16,
    hkeyClass: HKEY,
    dwHotKey: u32,
    hIconOrMonitor: HANDLE,
    hProcess: HANDLE,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(pid: u32) -> i32;
    fn WaitForSingleObject(handle: HANDLE, ms: u32) -> u32;
    fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> i32;
    fn CloseHandle(handle: HANDLE) -> i32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_len: u32,
    ) -> i32;
    fn GetDiskFreeSpaceExW(root: *const u16, free_to_caller: *mut u64, total: *mut u64, free: *mut u64) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(hwnd: HANDLE, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: HKEY,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        len: *mut u32,
    ) -> i32;
    fn RegOpenKeyExW(key: HKEY, subkey: *const u16, options: u32, access: u32, out: *mut HKEY) -> i32;
    fn RegCloseKey(key: HKEY) -> i32;
    fn RegSetKeyValueW(key: HKEY, subkey: *const u16, value: *const u16, kind: u32, data: *const c_void, len: u32) -> i32;
    fn RegDeleteKeyValueW(key: HKEY, subkey: *const u16, value: *const u16) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut SHELLEXECUTEINFOW) -> i32;
}

/// A NUL-terminated UTF-16 copy of a string.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

static CONSOLE: AtomicBool = AtomicBool::new(false);

/// Writes `--help`, `--version` and errors to the console this program
/// was started from (a window program has none of its own).
pub fn attach_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: no pointers involved.
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
    CONSOLE.store(attached, Ordering::Relaxed);
}

/// Shows an error in a message box, unless it already went to a console.
pub fn error_box(text: &str) {
    if CONSOLE.load(Ordering::Relaxed) {
        return;
    }
    let (text, caption) = (wide(text), wide("Fast search"));
    // SAFETY: NUL-terminated strings; MB_ICONERROR (0x10).
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10) };
}

/// A DWORD value from the registry.
pub fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    const RRF_RT_REG_DWORD: u32 = 0x10;
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut len = 4u32;
    // SAFETY: a 4-byte buffer for a DWORD, and its size.
    let r = unsafe {
        RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut len)
    };
    (r == 0).then_some(data)
}

/// True if a registry key exists (and may be read).
pub fn reg_key_exists(root: HKEY, key: &str) -> bool {
    const KEY_READ: u32 = 0x20019;
    let key = wide(key);
    let mut out: HKEY = 0;
    // SAFETY: valid string and out pointer.
    let r = unsafe { RegOpenKeyExW(root, key.as_ptr(), 0, KEY_READ, &mut out) };
    if r == 0 {
        // SAFETY: the key just opened.
        unsafe { RegCloseKey(out) };
    }
    r == 0
}

/// Runs a program with administrator rights (Windows asks the user first)
/// and waits for it. Returns its exit code.
pub fn run_elevated(exe: &Path, args: &[String]) -> io::Result<u32> {
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x40;
    const SEE_MASK_NO_CONSOLE: u32 = 0x8000;
    const SW_HIDE: i32 = 0;
    const ERROR_CANCELLED: i32 = 1223;
    let params: Vec<String> = args.iter().map(|a| quote_arg(a)).collect();
    let (verb, file, params) = (wide("runas"), wide(exe), wide(params.join(" ")));
    // SAFETY: all-zero is a valid (empty) value of this plain struct.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: `info` and its strings live through the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(ERROR_CANCELLED) { io::Error::other("not authorized") } else { e });
    }
    if info.hProcess.is_null() {
        return Ok(0);
    }
    let mut code = 1u32;
    // SAFETY: the process handle we asked for; closed once.
    unsafe {
        WaitForSingleObject(info.hProcess, u32::MAX);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    Ok(code)
}

/// A drive's filesystem name ("NTFS") and total size in bytes.
pub fn volume_info(root: &Path) -> (String, u64) {
    let root = wide(root);
    let mut fs_name = [0u16; 64];
    // SAFETY: a NUL-terminated root and a buffer with its true length.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    } != 0;
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(0);
    let name = if ok { String::from_utf16_lossy(&fs_name[..len]) } else { String::new() };
    let mut total = 0u64;
    // SAFETY: the root string and an out value; the other outs are optional.
    unsafe { GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total, std::ptr::null_mut()) };
    (name, total)
}

/// Quotes one argument the way the C runtime splits a command line.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// Creates or replaces a shell link (.lnk) to `target`, with a global
/// hot key such as "CTRL+ALT+F" (empty for none). Done by Windows'
/// scripting host, which ships with every version.
pub fn write_shortcut(link: &Path, target: &Path, hotkey: &str) -> io::Result<()> {
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let esc = |p: &Path| p.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.Description='Find files instantly by name';$s.Hotkey='{}';$s.Save()",
        esc(link),
        esc(target),
        hotkey.replace('\'', "")
    );
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()?;
    if out.status.success() && link.exists() {
        Ok(())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().lines().next().unwrap_or("could not create the shortcut").to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_the_c_runtime() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("C:\\My Files\\"), "\"C:\\My Files\\\\\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg(""), "\"\"");
    }
}

/// Sets a string value (REG_SZ) under `key`, creating the key if needed.
pub fn reg_set_string(root: HKEY, key: &str, value: &str, data: &str) -> std::io::Result<()> {
    const REG_SZ: u32 = 1;
    let (key, value, data) = (wide(key), wide(value), wide(data));
    // SAFETY: NUL-terminated strings; the data length counts its NUL.
    let r = unsafe { RegSetKeyValueW(root, key.as_ptr(), value.as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(r)) }
}

/// Removes a value; one that is not there is fine.
pub fn reg_delete_value(root: HKEY, key: &str, value: &str) -> std::io::Result<()> {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    let (key, value) = (wide(key), wide(value));
    // SAFETY: NUL-terminated strings.
    match unsafe { RegDeleteKeyValueW(root, key.as_ptr(), value.as_ptr()) } {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        r => Err(std::io::Error::from_raw_os_error(r)),
    }
}

/// True if a string value exists.
pub fn reg_string_exists(root: HKEY, key: &str, value: &str) -> bool {
    const RRF_RT_REG_SZ: u32 = 0x2;
    let (key, value) = (wide(key), wide(value));
    // SAFETY: no data buffer: only whether the value is there.
    let r = unsafe {
        RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    r == 0
}
