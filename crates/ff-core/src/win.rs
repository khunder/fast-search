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

#[repr(C)]
#[derive(Default)]
struct BY_HANDLE_FILE_INFORMATION {
    dwFileAttributes: u32,
    // FILETIMEs: two 32-bit halves, so 4-byte aligned.
    ftCreationTime: [u32; 2],
    ftLastAccessTime: [u32; 2],
    ftLastWriteTime: [u32; 2],
    dwVolumeSerialNumber: u32,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    nNumberOfLinks: u32,
    nFileIndexHigh: u32,
    nFileIndexLow: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDynamicTimeZoneInformation(info: *mut DYNAMIC_TIME_ZONE_INFORMATION) -> u32;
    fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, free: *mut u64) -> BOOL;
    fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> BOOL;
    fn GetVolumeNameForVolumeMountPointW(mount: *const u16, out: *mut u16, len: u32) -> BOOL;
    fn GetFileInformationByHandle(file: *mut c_void, info: *mut BY_HANDLE_FILE_INFORMATION) -> BOOL;
    fn DeviceIoControl(
        device: *mut c_void,
        code: u32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> BOOL;
}

fn from_wide(buf: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    std::ffi::OsString::from_wide(&buf[..len]).into()
}

/// The volume a folder is on: its mount point (`C:\`, or a folder where
/// a volume is mounted) and the device that opens it whole, such as
/// `\\?\Volume{...}` (no trailing backslash).
pub(crate) fn volume_of(dir: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let path = crate::os::wide(dir.as_os_str());
    let mut mount = [0u16; 1024];
    let mut name = [0u16; 128];
    // SAFETY: NUL-terminated input; output buffers with their lengths.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
        if GetVolumeNameForVolumeMountPointW(mount.as_ptr(), name.as_mut_ptr(), name.len() as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let device = from_wide(&name);
    // The volume name ends in a backslash, which would open its root
    // folder instead of the volume itself.
    let device = PathBuf::from(device.as_os_str().to_string_lossy().trim_end_matches('\\'));
    Ok((from_wide(&mount), device))
}

/// The NTFS file record number of an open file or folder.
pub(crate) fn file_record(file: &std::fs::File) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a valid handle and a properly sized output struct.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // The high 16 bits are the record's sequence number.
    Ok(((info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64) & 0xFFFF_FFFF_FFFF)
}

/// The number of the physical disk an open volume lies on, if it lies on
/// exactly one.
pub(crate) fn disk_number(volume: &std::fs::File) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
    // VOLUME_DISK_EXTENTS: a count (padded to 8), then 24-byte extents
    // that start with the disk number.
    let mut out = [0u64; 16];
    let mut returned = 0u32;
    // SAFETY: a valid handle; the output buffer and its size match.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            std::mem::size_of_val(&out) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    (ok != 0 && out[0] as u32 == 1).then_some(out[1] as u32)
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

/// The capacity and free space, in bytes, of the drive `dir` is on.
pub(crate) fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    let dir = crate::os::wide(dir.as_os_str());
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: a NUL-terminated path and three output values.
    let ok = unsafe { GetDiskFreeSpaceExW(dir.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0 && total > 0).then_some((total, free))
}

/// Hands `paths` to Windows to delete, as Explorer does: with Windows'
/// own confirmation (always asked before deleting for good), progress
/// window and errors. To the Recycle Bin unless `permanent`; Windows
/// warns about items too big for it. `owner` is the window the dialogs
/// belong to (0 for none). Returns false if the user cancelled.
pub fn shell_delete(paths: &[PathBuf], permanent: bool, owner: isize) -> io::Result<bool> {
    const FO_DELETE: u32 = 3;
    const FOF_ALLOWUNDO: u16 = 0x40;
    const FOF_WANTNUKEWARNING: u16 = 0x4000;
    // One list of full paths, each ending in a NUL, the list in two.
    let mut from = Vec::new();
    for path in paths {
        from.extend(crate::os::wide(std::path::absolute(path)?.as_os_str()));
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner as HWND,
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: if permanent { 0 } else { FOF_ALLOWUNDO | FOF_WANTNUKEWARNING },
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    // SAFETY: `op` and the list it points to live through the call.
    let r = unsafe { SHFileOperationW(&mut op) };
    match (r, op.fAnyOperationsAborted) {
        (_, 1..) => Ok(false),
        // 0x4C7 (ERROR_CANCELLED): the confirmation was answered No.
        (0x4C7, _) => Ok(false),
        (0, _) => Ok(true),
        (code, _) => Err(io::Error::other(format!("Windows could not delete everything (error {code})"))),
    }
}
