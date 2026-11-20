//! The handful of Windows API calls ff-core needs, declared by hand so
//! that no binding crate is required.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};

type BOOL = i32;
type HWND = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SYSTEMTIME {
    pub wYear: u16,
    pub wMonth: u16,
    pub wDayOfWeek: u16,
    pub wDay: u16,
    pub wHour: u16,
    pub wMinute: u16,
    pub wSecond: u16,
    pub wMilliseconds: u16,
}

#[repr(C)]
pub(crate) struct DYNAMIC_TIME_ZONE_INFORMATION {
    pub Bias: i32,
    pub StandardName: [u16; 32],
    pub StandardDate: SYSTEMTIME,
    pub StandardBias: i32,
    pub DaylightName: [u16; 32],
    pub DaylightDate: SYSTEMTIME,
    pub DaylightBias: i32,
    pub TimeZoneKeyName: [u16; 128],
    pub DynamicDaylightTimeDisabled: u8,
}

#[repr(C)]
struct SHFILEOPSTRUCTW {
    hwnd: HWND,
    wFunc: u32,
    pFrom: *const u16,
    pTo: *const u16,
    fFlags: u16,
    fAnyOperationsAborted: BOOL,
    hNameMappings: *mut c_void,
    lpszProgressTitle: *const u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDynamicTimeZoneInformation(info: *mut DYNAMIC_TIME_ZONE_INFORMATION) -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: HWND,
        verb: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
    fn SHFileOperationW(op: *mut SHFILEOPSTRUCTW) -> i32;
}

const DRIVE_FIXED: u32 = 3;
const SW_SHOWNORMAL: i32 = 1;

/// Every lettered drive that is a local fixed disk: `C:\`, `D:\`, ...
pub fn fixed_drives() -> Vec<PathBuf> {
    // SAFETY: no arguments; returns a bit mask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| {
            let w: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated root path.
            unsafe { GetDriveTypeW(w.as_ptr()) == DRIVE_FIXED }
        })
        .map(PathBuf::from)
        .collect()
}

/// The machine's time zone and its daylight-saving rule.
pub(crate) fn time_zone() -> Option<DYNAMIC_TIME_ZONE_INFORMATION> {
    // SAFETY: plain data; zeroed is a valid value, then filled in.
    let mut info: DYNAMIC_TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let r = unsafe { GetDynamicTimeZoneInformation(&mut info) };
    (r != u32::MAX).then_some(info)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Runs a shell verb ("open", "edit", ...) on a file, like double-clicking
/// it in Explorer.
pub fn shell_execute(verb: &str, path: &Path) -> io::Result<()> {
    let verb = wide(verb);
    let file = crate::os::wide(path.as_os_str());
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // Values above 32 mean success; below are error codes.
    if r > 32 { Ok(()) } else { Err(io::Error::other(format!("cannot open {} (error {r})", path.display()))) }
}

/// Moves a file or folder to the Recycle Bin. Where it cannot go there
/// (a drive without a Recycle Bin, such as a network share or many USB
/// sticks, or an item too big for it), Windows asks first instead of
/// deleting it for good; saying no is an error.
pub fn recycle(path: &Path) -> io::Result<()> {
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x4;
    const FOF_NOCONFIRMATION: u16 = 0x10;
    const FOF_ALLOWUNDO: u16 = 0x40;
    const FOF_NOERRORUI: u16 = 0x400;
    const FOF_WANTNUKEWARNING: u16 = 0x4000;
    // The API takes a list ending in two NULs, and wants a full path.
    let full = std::path::absolute(path)?;
    let mut from = crate::os::wide(full.as_os_str());
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    // SAFETY: `op` and the strings it points to live through the call.
    let r = unsafe { SHFileOperationW(&mut op) };
    if r == 0 && op.fAnyOperationsAborted == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("could not move {} to the Recycle Bin (error {r})", path.display())))
    }
}

/// Shows `path` selected in an Explorer window.
pub fn reveal(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let full = std::path::absolute(path)?;
    // Explorer parses its own command line: the path must be quoted
    // after the comma, which the usual argument quoting would not do.
    let mut arg = std::ffi::OsString::from("/select,\"");
    arg.push(full.as_os_str());
    arg.push("\"");
    // Explorer's exit code means nothing (it is 1 even on success).
    std::process::Command::new("explorer.exe").raw_arg(&arg).spawn().map(|_| ())
}

/// The value of a `FILE_ATTRIBUTE_*` set, as words.
pub fn attribute_names(attrs: u32) -> String {
    let names = [
        (0x1, "read-only"),
        (0x2, "hidden"),
        (0x4, "system"),
        (0x20, "archive"),
        (0x400, "reparse point"),
        (0x800, "compressed"),
        (0x4000, "encrypted"),
        (0x1000, "offline"),
    ];
    let set: Vec<&str> = names.iter().filter(|(bit, _)| attrs & bit != 0).map(|&(_, n)| n).collect();
    if set.is_empty() { "normal".into() } else { set.join(", ") }
}
