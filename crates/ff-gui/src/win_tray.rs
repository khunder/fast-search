//! Windows: the notification-area ("tray") icon the window hides to when
//! it is closed, and the hand-over that lets a second start of the
//! program (such as the Ctrl+Alt+F shortcut) show the running window
//! instead of loading the index again.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;

type HWND = *mut c_void;
type HANDLE = *mut c_void;
type BOOL = i32;

/// Sent by the tray icon on clicks (`lparam` says which).
pub const TRAY_MESSAGE: u32 = 0x8000 + 1; // WM_APP + 1
/// Sent by a second start of the program: show yourself.
pub const SHOW_MESSAGE: u32 = 0x8000 + 2; // WM_APP + 2

/// The main window's class, by which (with its title) a second start
/// finds it.
pub const CLASS: &str = "ff-gui";

#[repr(C)]
struct NOTIFYICONDATAW {
    cbSize: u32,
    hWnd: HWND,
    uID: u32,
    uFlags: u32,
    uCallbackMessage: u32,
    hIcon: HANDLE,
    szTip: [u16; 128],
    dwState: u32,
    dwStateMask: u32,
    szInfo: [u16; 256],
    uVersion: u32,
    szInfoTitle: [u16; 64],
    dwInfoFlags: u32,
    guidItem: [u8; 16],
    hBalloonIcon: HANDLE,
}

#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn Shell_NotifyIconW(message: u32, data: *const NOTIFYICONDATAW) -> BOOL;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn FindWindowW(class: *const u16, title: *const u16) -> HWND;
    fn PostMessageW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> BOOL;
    fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    fn AllowSetForegroundWindow(pid: u32) -> BOOL;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn CreatePopupMenu() -> HANDLE;
    fn AppendMenuW(menu: HANDLE, flags: u32, id: usize, text: *const u16) -> BOOL;
    fn TrackPopupMenu(menu: HANDLE, flags: u32, x: i32, y: i32, reserved: i32, hwnd: HWND, rect: *const c_void) -> BOOL;
    fn DestroyMenu(menu: HANDLE) -> BOOL;
    fn GetCursorPos(point: *mut POINT) -> BOOL;
    fn SetForegroundWindow(hwnd: HWND) -> BOOL;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: plain data; all zero is a valid starting value.
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = 1;
    d
}

/// Puts the icon in the notification area, with `icon` and a tooltip.
pub fn add(hwnd: HWND, icon: HANDLE) {
    const NIM_ADD: u32 = 0;
    const NIF_MESSAGE: u32 = 0x1;
    const NIF_ICON: u32 = 0x2;
    const NIF_TIP: u32 = 0x4;
    let mut d = data(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = TRAY_MESSAGE;
    d.hIcon = icon;
    let tip = wide("Fast search");
    d.szTip[..tip.len()].copy_from_slice(&tip);
    // SAFETY: a filled-in, properly sized structure.
    unsafe { Shell_NotifyIconW(NIM_ADD, &d) };
}

pub fn remove(hwnd: HWND) {
    const NIM_DELETE: u32 = 2;
    // SAFETY: as above; only the window and id are needed.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data(hwnd)) };
}

/// The message Explorer broadcasts when it (re)starts: tray icons must
/// then be added again.
pub fn taskbar_created() -> u32 {
    // SAFETY: a NUL-terminated name.
    unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }
}

/// What the tray icon's menu asked for.
pub enum Choice {
    Open,
    Exit,
}

/// Shows the tray icon's menu at the pointer.
pub fn menu(hwnd: HWND) -> Option<Choice> {
    const MF_STRING: u32 = 0;
    const TPM_RIGHTBUTTON: u32 = 0x2;
    const TPM_RETURNCMD: u32 = 0x100;
    let (open, exit) = (wide(crate::i18n::tr("Open Fast search")), wide(crate::i18n::tr("Exit")));
    // SAFETY: a menu created and destroyed here; strings outlive it.
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, 1, open.as_ptr());
        AppendMenuW(menu, MF_STRING, 2, exit.as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        // Without this the menu does not close when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, at.x, at.y, 0, hwnd, std::ptr::null());
        DestroyMenu(menu);
        match chosen {
            1 => Some(Choice::Open),
            2 => Some(Choice::Exit),
            _ => None,
        }
    }
}

/// If the program already runs, asks its window to show itself and
/// returns true: this start then has nothing more to do.
pub fn wake_running(title: &str) -> bool {
    let (class, title) = (wide(CLASS), wide(title));
    // SAFETY: NUL-terminated strings; the window may be hidden, which
    // FindWindowW still finds.
    unsafe {
        let hwnd = FindWindowW(class.as_ptr(), title.as_ptr());
        if hwnd.is_null() {
            return false;
        }
        // Started by the user, this process may bring a window forward;
        // pass that right on to the running one.
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        AllowSetForegroundWindow(pid);
        PostMessageW(hwnd, SHOW_MESSAGE, 0, 0) != 0
    }
}

/// True if the program already runs (its window may be hidden).
pub fn running(title: &str) -> bool {
    let (class, title) = (wide(CLASS), wide(title));
    // SAFETY: NUL-terminated strings.
    !unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) }.is_null()
}
