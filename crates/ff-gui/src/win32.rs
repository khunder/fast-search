//! The Windows backend: the application's windows, input, clipboard and
//! drag and drop over plain Win32 (user32, gdi32, ole32, shell32),
//! declared by hand. The window content is drawn by the application and
//! copied to the screen as a device-independent bitmap.
//!
//! Window messages are turned into `Event`s by the window procedure and
//! queued in thread-local state; `next_event` pumps the message queue.
//! Keys are translated in the pump itself, before `TranslateMessage`, so
//! that Ctrl and Alt combinations arrive as the letters they were typed
//! on rather than as control characters.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::backend::{Backend, Cursor, Event, Key, Mods, Win, WindowSpec};
use crate::canvas::{Canvas, Icon};
use crate::keys::*;
use crate::winsys::wide;

type HWND = *mut c_void;
type HANDLE = *mut c_void;
type WPARAM = usize;
type LPARAM = isize;
type LRESULT = isize;
type BOOL = i32;
type WNDPROC = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct WNDCLASSEXW {
    cbSize: u32,
    style: u32,
    lpfnWndProc: WNDPROC,
    cbClsExtra: i32,
    cbWndExtra: i32,
    hInstance: HANDLE,
    hIcon: HANDLE,
    hCursor: HANDLE,
    hbrBackground: HANDLE,
    lpszMenuName: *const u16,
    lpszClassName: *const u16,
    hIconSm: HANDLE,
}

#[repr(C)]
struct MSG {
    hwnd: HWND,
    message: u32,
    wParam: WPARAM,
    lParam: LPARAM,
    time: u32,
    pt: POINT,
    lPrivate: u32,
}

#[repr(C)]
struct PAINTSTRUCT {
    hdc: HANDLE,
    fErase: BOOL,
    rcPaint: RECT,
    fRestore: BOOL,
    fIncUpdate: BOOL,
    rgbReserved: [u8; 32],
}

#[repr(C)]
struct BITMAPINFOHEADER {
    biSize: u32,
    biWidth: i32,
    biHeight: i32,
    biPlanes: u16,
    biBitCount: u16,
    biCompression: u32,
    biSizeImage: u32,
    biXPelsPerMeter: i32,
    biYPelsPerMeter: i32,
    biClrUsed: u32,
    biClrImportant: u32,
}

#[repr(C)]
struct ICONINFO {
    fIcon: BOOL,
    xHotspot: u32,
    yHotspot: u32,
    hbmMask: HANDLE,
    hbmColor: HANDLE,
}

#[repr(C)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct DROPFILES {
    pFiles: u32,
    pt: POINT,
    fNC: BOOL,
    fWide: BOOL,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WNDCLASSEXW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HANDLE,
        instance: HANDLE,
        param: *mut c_void,
    ) -> HWND;
    fn DefWindowProcW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    fn DestroyWindow(hwnd: HWND) -> BOOL;
    fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
    fn PeekMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32, remove: u32) -> BOOL;
    fn TranslateMessage(msg: *const MSG) -> BOOL;
    fn DispatchMessageW(msg: *const MSG) -> LRESULT;
    fn MsgWaitForMultipleObjectsEx(count: u32, handles: *const HANDLE, ms: u32, wake: u32, flags: u32) -> u32;
    fn GetDC(hwnd: HWND) -> HANDLE;
    fn ReleaseDC(hwnd: HWND, dc: HANDLE) -> i32;
    fn BeginPaint(hwnd: HWND, ps: *mut PAINTSTRUCT) -> HANDLE;
    fn EndPaint(hwnd: HWND, ps: *const PAINTSTRUCT) -> BOOL;
    fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    fn GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    fn SetWindowPos(hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: u32) -> BOOL;
    fn AdjustWindowRectExForDpi(rect: *mut RECT, style: u32, menu: BOOL, ex_style: u32, dpi: u32) -> BOOL;
    fn GetDpiForWindow(hwnd: HWND) -> u32;
    fn GetDpiForSystem() -> u32;
    fn SetProcessDpiAwarenessContext(context: isize) -> BOOL;
    fn LoadCursorW(instance: HANDLE, name: *const u16) -> HANDLE;
    fn SetCursor(cursor: HANDLE) -> HANDLE;
    fn SetCapture(hwnd: HWND) -> HWND;
    fn ReleaseCapture() -> BOOL;
    fn SendMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    fn OpenClipboard(hwnd: HWND) -> BOOL;
    fn CloseClipboard() -> BOOL;
    fn EmptyClipboard() -> BOOL;
    fn SetClipboardData(format: u32, data: HANDLE) -> HANDLE;
    fn GetClipboardData(format: u32) -> HANDLE;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn GetKeyState(vk: i32) -> i16;
    fn GetKeyboardState(state: *mut u8) -> BOOL;
    fn ToUnicode(vk: u32, scan: u32, state: *const u8, buf: *mut u16, len: i32, flags: u32) -> i32;
    fn GetMessageTime() -> i32;
    fn EnableWindow(hwnd: HWND, enable: BOOL) -> BOOL;
    fn SetForegroundWindow(hwnd: HWND) -> BOOL;
    fn IsZoomed(hwnd: HWND) -> BOOL;
    fn CreateIconIndirect(info: *const ICONINFO) -> HANDLE;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn SetDIBitsToDevice(
        dc: HANDLE,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        src_x: i32,
        src_y: i32,
        start_scan: u32,
        lines: u32,
        bits: *const c_void,
        info: *const BITMAPINFOHEADER,
        usage: u32,
    ) -> i32;
    fn CreateBitmap(w: i32, h: i32, planes: u32, bits_per_pixel: u32, bits: *const c_void) -> HANDLE;
    fn DeleteObject(object: HANDLE) -> BOOL;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> HANDLE;
    fn GlobalAlloc(flags: u32, bytes: usize) -> HANDLE;
    fn GlobalLock(mem: HANDLE) -> *mut c_void;
    fn GlobalUnlock(mem: HANDLE) -> BOOL;
    fn GlobalFree(mem: HANDLE) -> HANDLE;
    fn GlobalSize(mem: HANDLE) -> usize;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn OleInitialize(reserved: *mut c_void) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ILCreateFromPathW(path: *const u16) -> *mut c_void;
    fn ILFree(pidl: *mut c_void);
    fn SHCreateDataObject(
        folder: *const c_void,
        count: u32,
        items: *const *const c_void,
        inner: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> i32;
    fn SHDoDragDrop(hwnd: HWND, data: *mut c_void, source: *mut c_void, effects: u32, effect: *mut u32) -> i32;
    fn DragQueryFileW(drop: HANDLE, index: u32, buf: *mut u16, len: u32) -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> HANDLE;
    fn GetProcAddress(module: HANDLE, name: *const u8) -> *const c_void;
}

/// Gives a window a dark title bar when Windows is in dark mode, to
/// match the dark palette the application then draws with. dwmapi is
/// loaded at run time: the GNU toolchain ships no import library for it.
fn match_title_bar(hwnd: HWND) {
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    type SetAttribute = unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> i32;
    let dark: BOOL = crate::settings::desktop_prefers_dark() as BOOL;
    // SAFETY: a system library; the function has this signature.
    unsafe {
        let dwm = LoadLibraryW(wide("dwmapi.dll").as_ptr());
        let set = if dwm.is_null() { std::ptr::null() } else { GetProcAddress(dwm, c"DwmSetWindowAttribute".as_ptr().cast()) };
        if !set.is_null() {
            let set: SetAttribute = std::mem::transmute(set);
            set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const BOOL).cast(), 4);
        }
    }
}

// Messages.
const WM_DESTROY: u32 = 0x0002;
const WM_SIZE: u32 = 0x0005;
const WM_PAINT: u32 = 0x000F;
const WM_CLOSE: u32 = 0x0010;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETCURSOR: u32 = 0x0020;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_SETICON: u32 = 0x0080;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_KEYDOWN: u32 = 0x0100;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_SYSCHAR: u32 = 0x0106;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_DPICHANGED: u32 = 0x02E0;

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_POPUP: u32 = 0x8000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_EX_DLGMODALFRAME: u32 = 0x0000_0001;
const CW_USEDEFAULT: i32 = i32::MIN;

const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SW_MINIMIZE: i32 = 6;
const SW_MAXIMIZE: i32 = 3;
const SW_RESTORE: i32 = 9;
const SWP_NOZORDER: u32 = 0x4;
const SWP_NOACTIVATE: u32 = 0x10;

const HTCLIENT: u16 = 1;
const HTCAPTION: usize = 2;

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_RMENU: i32 = 0xA5;
const VK_F4: usize = 0x73;
const VK_F10: usize = 0x79;

const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const GMEM_MOVEABLE: u32 = 0x2;

const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;
const DRAGDROP_S_DROP: i32 = 0x0004_0100;

const IID_IDATAOBJECT: GUID =
    GUID { data1: 0x0000_010e, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

/// A frame shown in a window, kept to repaint it without the application.
struct Frame {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

/// What the window procedure shares with the backend.
#[derive(Default)]
struct Shared {
    events: VecDeque<Event>,
    main: usize,
    dialog: usize,
    size: (usize, usize),
    frames: [Option<Frame>; 2],
    cursor: usize,
    /// The first half of a character outside the Basic Multilingual Plane.
    high_surrogate: u16,
    /// Wheel movement not yet turned into whole lines.
    wheel: i32,
    scale: f32,
    /// The program, not the desktop, chose the scale.
    fixed_scale: bool,
}

thread_local! {
    static SHARED: RefCell<Shared> = RefCell::new(Shared::default());
}

/// Runs `f` on the shared state. Never call into Windows inside `f`:
/// that can re-enter the window procedure.
fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    SHARED.with(|s| f(&mut s.borrow_mut()))
}

fn push(event: Event) {
    with(|s| s.events.push_back(event));
}

fn win_of(hwnd: HWND) -> Option<Win> {
    with(|s| {
        if hwnd as usize == s.main && s.main != 0 {
            Some(Win::Main)
        } else if hwnd as usize == s.dialog && s.dialog != 0 {
            Some(Win::Dialog)
        } else {
            None
        }
    })
}

fn down(vk: i32) -> bool {
    // SAFETY: no pointers.
    unsafe { GetKeyState(vk) < 0 }
}

fn mods() -> Mods {
    Mods { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU) }
}

fn lo(l: LPARAM) -> i32 {
    l as u16 as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    (l >> 16) as u16 as i16 as i32
}

fn client_size(hwnd: HWND) -> (usize, usize) {
    let mut r = RECT::default();
    // SAFETY: a valid window and an out rectangle.
    unsafe { GetClientRect(hwnd, &mut r) };
    ((r.right - r.left).max(0) as usize, (r.bottom - r.top).max(0) as usize)
}

/// Copies a frame to a device context.
fn blit(dc: HANDLE, frame: &Frame) {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: frame.w as i32,
        biHeight: -(frame.h as i32), // top-down rows, like the canvas
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0, // BI_RGB: the canvas's BGRX bytes as they are
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    // SAFETY: `px` holds w * h * 4 bytes described by `header`.
    unsafe {
        SetDIBitsToDevice(dc, 0, 0, frame.w as u32, frame.h as u32, 0, 0, 0, frame.h as u32, frame.px.as_ptr().cast(), &header, 0)
    };
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY (for the calls below): `hwnd` is the window being served.
    let Some(win) = win_of(hwnd) else { return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) } };
    match msg {
        WM_CLOSE => push(Event::Close(win)),
        WM_DESTROY => {}
        WM_ERASEBKGND => return 1, // every pixel is painted by us
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let dc = unsafe { BeginPaint(hwnd, &mut ps) };
            let size = client_size(hwnd);
            let frame = with(|s| s.frames[win as usize].take());
            let stale = frame.as_ref().is_none_or(|f| (f.w, f.h) != size);
            if let Some(frame) = frame {
                blit(dc, &frame);
                with(|s| s.frames[win as usize] = Some(frame));
            }
            unsafe { EndPaint(hwnd, &ps) };
            if stale {
                push(Event::Redraw(win));
            }
        }
        WM_SIZE if win == Win::Main => {
            let size = (lo(lparam) as u16 as usize, hi(lparam) as u16 as usize);
            // Zero while minimized: nothing to draw then.
            if size.0 > 0 && size.1 > 0 {
                let changed = with(|s| std::mem::replace(&mut s.size, size) != size);
                if changed {
                    push(Event::Resize(size.0, size.1));
                }
            }
        }
        WM_GETMINMAXINFO => {
            // ptMinTrackSize: keep the window big enough to use.
            let scale = with(|s| s.scale).max(1.0);
            let info = lparam as *mut POINT;
            unsafe {
                (*info.add(3)).x = (360.0 * scale) as i32;
                (*info.add(3)).y = (240.0 * scale) as i32;
            }
        }
        WM_SETCURSOR if (lparam as u16) == HTCLIENT && win == Win::Main => {
            let cursor = with(|s| s.cursor);
            unsafe { SetCursor(cursor as HANDLE) };
            return 1;
        }
        WM_DPICHANGED => {
            let dpi = (wparam & 0xffff) as f32;
            let r = unsafe { *(lparam as *const RECT) };
            unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            if win == Win::Main {
                let scale = dpi / 96.0;
                let fixed = with(|s| {
                    s.scale = scale;
                    s.fixed_scale
                });
                if !fixed {
                    push(Event::Scale(scale));
                }
            }
        }
        WM_CHAR => {
            let unit = wparam as u16;
            let ch = match unit {
                0xD800..=0xDBFF => {
                    with(|s| s.high_surrogate = unit);
                    None
                }
                0xDC00..=0xDFFF => {
                    let high = with(|s| std::mem::take(&mut s.high_surrogate));
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                }
                _ => char::from_u32(unit as u32),
            };
            // Control characters arrive as keys from the key-down message.
            if let Some(ch) = ch.filter(|c| !c.is_control()) {
                // Text typed with AltGr is plain text, not a shortcut.
                push(Event::Key { win, key: Key::Char(ch), mods: Mods { shift: down(VK_SHIFT), ctrl: false, alt: false } });
            }
        }
        WM_SYSCHAR => {} // no menu bar to beep about
        WM_MOUSEMOVE => push(Event::Motion { win, x: lo(lparam), y: hi(lparam), mods: mouse_mods(wparam) }),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            unsafe { SetCapture(hwnd) };
            let button = match msg {
                WM_LBUTTONDOWN => 1,
                WM_MBUTTONDOWN => 2,
                _ => 3,
            };
            let time = unsafe { GetMessageTime() } as u32;
            push(Event::Press { win, button, x: lo(lparam), y: hi(lparam), time, mods: mouse_mods(wparam) });
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            // MK_LBUTTON | MK_RBUTTON | MK_MBUTTON: none still held.
            if wparam & 0x13 == 0 {
                unsafe { ReleaseCapture() };
            }
            let button = match msg {
                WM_LBUTTONUP => 1,
                WM_MBUTTONUP => 2,
                _ => 3,
            };
            push(Event::Release { win, button });
        }
        WM_MOUSEWHEEL => {
            // 120 per notch; three lines a notch, finer for touchpads.
            let delta = hi(wparam as isize);
            let lines = with(|s| {
                s.wheel += delta;
                let lines = s.wheel / 40;
                s.wheel -= lines * 40;
                lines
            });
            if lines != 0 {
                push(Event::Scroll { win, lines: -lines });
            }
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

fn mouse_mods(wparam: WPARAM) -> Mods {
    Mods { shift: wparam & 0x4 != 0, ctrl: wparam & 0x8 != 0, alt: down(VK_MENU) }
}

/// The keysym for a virtual key that is not text.
fn special_key(vk: usize, extended: bool, shift: bool) -> Option<u32> {
    Some(match vk {
        0x08 => K_BACKSPACE,
        0x09 if shift => K_BACK_TAB,
        0x09 => K_TAB,
        0x0D if extended => K_KP_ENTER,
        0x0D => K_RETURN,
        0x1B => K_ESCAPE,
        0x21 => K_PAGE_UP,
        0x22 => K_PAGE_DOWN,
        0x23 => K_END,
        0x24 => K_HOME,
        0x25 => K_LEFT,
        0x26 => K_UP,
        0x27 => K_RIGHT,
        0x28 => K_DOWN,
        0x2D => 0xff63, // Insert
        0x2E => K_DELETE,
        0x5D => K_MENU,
        0x70..=0x7B => K_F1 + (vk - 0x70) as u32,
        _ => return None,
    })
}

/// The character a key gives with Ctrl and Alt let go, Shift as held.
fn key_char(vk: u32, scan: u32) -> Option<char> {
    let mut state = [0u8; 256];
    // SAFETY: a 256-byte buffer, as required.
    unsafe { GetKeyboardState(state.as_mut_ptr()) };
    for k in [VK_CONTROL, VK_MENU, 0xA2, 0xA3, 0xA4, VK_RMENU] {
        state[k as usize] = 0;
    }
    let mut buf = [0u16; 4];
    // Flag 4: leave the keyboard state (dead keys) alone.
    let n = unsafe { ToUnicode(vk, scan, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, 4) };
    if n <= 0 {
        return None;
    }
    char::decode_utf16(buf[..n as usize].iter().copied()).next().and_then(Result::ok).filter(|c| !c.is_control())
}

/// Handles a key-down message before translation. True if it was used
/// up here; false to let it become text (`WM_CHAR`) as usual.
fn key_down(msg: &MSG) -> bool {
    let Some(win) = win_of(msg.hwnd) else { return false };
    let vk = msg.wParam;
    let m = mods();
    let altgr = m.ctrl && m.alt && down(VK_RMENU);
    match vk {
        // Shift, Ctrl, Alt, Caps Lock, Windows keys, Num/Scroll Lock.
        0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0x90 | 0x91 | 0xA0..=0xA5 => return true,
        // Alt+F4 closes the window, as everywhere.
        VK_F4 if m.alt => return false,
        _ => {}
    }
    let extended = msg.lParam & (1 << 24) != 0;
    if let Some(sym) = special_key(vk, extended, m.shift) {
        push(Event::Key { win, key: Key::Sym(sym), mods: m });
        return true;
    }
    if (m.ctrl || m.alt) && !altgr {
        let scan = ((msg.lParam >> 16) & 0xff) as u32;
        if let Some(ch) = key_char(vk as u32, scan) {
            // Letters as X reports them: lowercase unless Shift is held.
            let ch = if m.shift { ch } else { ch.to_lowercase().next().unwrap_or(ch) };
            push(Event::Key { win, key: Key::Char(ch), mods: m });
        }
        return true;
    }
    false
}

/// Puts `data` in a movable global memory block for the clipboard.
fn global_bytes(data: &[u8]) -> Option<HANDLE> {
    // SAFETY: a fresh block of the right size, filled while locked.
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
        if mem.is_null() {
            return None;
        }
        let at = GlobalLock(mem);
        if at.is_null() {
            GlobalFree(mem);
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), at.cast(), data.len());
        GlobalUnlock(mem);
        Some(mem)
    }
}

fn utf16_bytes(units: impl IntoIterator<Item = u16>) -> Vec<u8> {
    units.into_iter().flat_map(u16::to_le_bytes).collect()
}

/// Opens the clipboard, which another program may hold for a moment.
fn open_clipboard(hwnd: HWND) -> bool {
    for _ in 0..10 {
        // SAFETY: our window.
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Reads the clipboard as text: text itself, or copied files as paths.
fn clipboard_text(hwnd: HWND) -> Option<String> {
    if !open_clipboard(hwnd) {
        return None;
    }
    // SAFETY: handles from the clipboard, read while it is open and locked.
    let text = unsafe {
        let mem = GetClipboardData(CF_UNICODETEXT);
        if !mem.is_null() {
            let at = GlobalLock(mem) as *const u16;
            if at.is_null() {
                None
            } else {
                // Up to the first NUL, but never past the block: another
                // program may have left the terminator out.
                let room = GlobalSize(mem) / 2;
                let mut len = 0;
                while len < room && *at.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(at, len));
                GlobalUnlock(mem);
                Some(text)
            }
        } else {
            let drop = GetClipboardData(CF_HDROP);
            (!drop.is_null()).then(|| {
                let count = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
                let paths: Vec<String> = (0..count)
                    .map(|i| {
                        let len = DragQueryFileW(drop, i, std::ptr::null_mut(), 0) as usize;
                        let mut buf = vec![0u16; len + 1];
                        DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
                        String::from_utf16_lossy(&buf[..len])
                    })
                    .collect();
                paths.join("\n")
            })
        }
    };
    // SAFETY: we opened it.
    unsafe { CloseClipboard() };
    text
}

/// A shell data object offering `paths` as files (to Explorer and any
/// program that takes dropped or pasted files). Released by the caller
/// through its vtable.
fn files_data_object(paths: &[&Path]) -> Option<*mut c_void> {
    let pidls: Vec<*mut c_void> = paths
        .iter()
        .map(|p| {
            let w = wide(p);
            // SAFETY: a NUL-terminated path.
            unsafe { ILCreateFromPathW(w.as_ptr()) }
        })
        .filter(|p| !p.is_null())
        .collect();
    if pidls.is_empty() {
        return None;
    }
    // Full item ids are relative to the desktop, the empty id list.
    let desktop = [0u16; 1];
    let mut object: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid id lists, freed below after the object copied them.
    let hr = unsafe {
        SHCreateDataObject(
            desktop.as_ptr().cast(),
            pidls.len() as u32,
            pidls.as_ptr() as *const *const c_void,
            std::ptr::null_mut(),
            &IID_IDATAOBJECT,
            &mut object,
        )
    };
    for pidl in pidls {
        // SAFETY: created above.
        unsafe { ILFree(pidl) };
    }
    (hr >= 0 && !object.is_null()).then_some(object)
}

/// Calls IUnknown::Release on a COM object.
fn release(object: *mut c_void) {
    // SAFETY: a live COM object; Release is the third vtable entry.
    unsafe {
        let vtable = *(object as *const *const usize);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtable.add(2));
        release(object);
    }
}

/// Makes a window icon from the application's drawn icon.
fn make_icon(size: usize) -> HANDLE {
    let pixels: Vec<u8> = Icon::App.render(size).into_iter().flatten().collect();
    let mask = vec![0u8; size.div_ceil(16) * 2 * size];
    // SAFETY: buffers of the sizes the bitmaps are made with.
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, pixels.as_ptr().cast());
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

pub struct Win32 {
    main: HWND,
    dialog: HWND,
    class: Vec<u16>,
    instance: HANDLE,
    cursor: Cursor,
}

impl Win32 {
    pub fn connect(spec: &WindowSpec) -> io::Result<Win32> {
        // SAFETY (whole function): plain Win32 calls with valid arguments.
        unsafe {
            // Per-monitor DPI awareness (v2): we scale ourselves, sharply.
            SetProcessDpiAwarenessContext(-4);
            OleInitialize(std::ptr::null_mut());
            let instance = GetModuleHandleW(std::ptr::null());
            let class = wide("ff-gui");
            let arrow = LoadCursorW(std::ptr::null_mut(), 32512 as *const u16);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: 0x3, // CS_HREDRAW | CS_VREDRAW
                lpfnWndProc: window_proc,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: arrow,
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);
            with(|s| s.cursor = arrow as usize);

            let title = wide(spec.title);
            let main = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if main.is_null() {
                return Err(io::Error::last_os_error());
            }
            // The monitor the window landed on decides the scale.
            let dpi = match GetDpiForWindow(main) {
                0 => GetDpiForSystem(),
                dpi => dpi,
            };
            let desktop = dpi as f32 / 96.0;
            let scale = spec.scale.unwrap_or(desktop);
            with(|s| {
                s.main = main as usize;
                s.scale = desktop;
                s.fixed_scale = spec.scale.is_some();
            });
            let mut r = RECT { left: 0, top: 0, right: (spec.width as f32 * scale) as i32, bottom: (spec.height as f32 * scale) as i32 };
            AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, 0, 0, dpi);
            SetWindowPos(main, std::ptr::null_mut(), 0, 0, r.right - r.left, r.bottom - r.top, 0x2 | SWP_NOZORDER); // SWP_NOMOVE
            for (kind, size) in [(1, 48), (0, 16 * (scale.round() as usize).max(1))] {
                SendMessageW(main, WM_SETICON, kind, make_icon(size) as LPARAM);
            }
            match_title_bar(main);
            ShowWindow(main, SW_SHOW);
            let size = client_size(main);
            with(|s| {
                s.size = size;
                s.events.push_back(Event::Resize(size.0.max(1), size.1.max(1)));
            });
            Ok(Win32 { main, dialog: std::ptr::null_mut(), class, instance, cursor: Cursor::Arrow })
        }
    }

    fn hwnd(&self, win: Win) -> HWND {
        match win {
            Win::Main => self.main,
            Win::Dialog => self.dialog,
        }
    }

    /// Handles every message waiting in the queue.
    fn pump(&mut self) {
        // SAFETY: a zeroed MSG is valid; filled by PeekMessage.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, 1) } != 0 {
            match msg.message {
                WM_KEYDOWN | WM_SYSKEYDOWN if key_down(&msg) => continue,
                // Alt or F10 alone would select the (absent) menu bar.
                WM_SYSKEYUP if msg.wParam == VK_MENU as usize || msg.wParam == VK_F10 => continue,
                _ => {}
            }
            // SAFETY: a message just taken from our queue.
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Hands the pressed mouse button to Windows for a move or resize of
    /// the main window (which returns once the button is let go).
    fn system_drag(&mut self, hit: usize) {
        // SAFETY: our window.
        unsafe {
            ReleaseCapture();
            SendMessageW(self.main, WM_NCLBUTTONDOWN, hit, 0);
        }
        push(Event::Release { win: Win::Main, button: 1 });
    }
}

impl Drop for Win32 {
    fn drop(&mut self) {
        self.close_dialog();
        with(|s| s.main = 0);
        // SAFETY: our window, destroyed once.
        unsafe { DestroyWindow(self.main) };
    }
}

impl Backend for Win32 {
    fn name(&self) -> &'static str {
        "Win32"
    }

    fn scale(&mut self) -> f32 {
        with(|s| s.scale)
    }

    fn next_event(&mut self, wait: Option<Duration>) -> io::Result<Option<Event>> {
        let deadline = wait.map(|w| Instant::now() + w);
        loop {
            if let Some(event) = with(|s| s.events.pop_front()) {
                return Ok(Some(event));
            }
            self.pump();
            if let Some(event) = with(|s| s.events.pop_front()) {
                return Ok(Some(event));
            }
            let ms = match deadline {
                None => u32::MAX,
                Some(at) => {
                    let now = Instant::now();
                    if now >= at {
                        return Ok(None);
                    }
                    (at - now).as_millis().clamp(1, u32::MAX as u128 - 1) as u32
                }
            };
            // QS_ALLINPUT, MWMO_INPUTAVAILABLE.
            // SAFETY: no handles to wait on besides the message queue.
            unsafe { MsgWaitForMultipleObjectsEx(0, std::ptr::null(), ms, 0x4FF, 0x4) };
        }
    }

    fn present(&mut self, win: Win, canvas: &Canvas) -> io::Result<()> {
        let hwnd = self.hwnd(win);
        if hwnd.is_null() || canvas.w == 0 || canvas.h == 0 {
            return Ok(());
        }
        let frame = Frame { w: canvas.w, h: canvas.h, px: canvas.px.clone() };
        // SAFETY: our window's device context, released after use.
        unsafe {
            let dc = GetDC(hwnd);
            blit(dc, &frame);
            ReleaseDC(hwnd, dc);
        }
        with(|s| s.frames[win as usize] = Some(frame));
        Ok(())
    }

    fn set_own_title_bar(&mut self, _own: bool) -> bool {
        false // Windows draws the frame, with its snap layouts and all
    }

    fn dialog_needs_title_bar(&self) -> bool {
        false
    }

    fn start_move(&mut self) {
        self.system_drag(HTCAPTION);
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        if std::mem::replace(&mut self.cursor, cursor) == cursor {
            return;
        }
        let id: usize = match cursor {
            Cursor::Arrow => 32512,
            Cursor::Text => 32513,
            // top-left, top, top-right, right, bottom-right, bottom, bottom-left, left
            Cursor::Resize(edge) => [32642, 32645, 32643, 32644, 32642, 32645, 32643, 32644][edge as usize % 8],
        };
        // SAFETY: a system cursor; shared, never freed.
        let handle = unsafe { LoadCursorW(std::ptr::null_mut(), id as *const u16) };
        with(|s| s.cursor = handle as usize);
        // SAFETY: a cursor handle.
        unsafe { SetCursor(handle) };
    }

    fn start_resize(&mut self, edge: u32) {
        // HTTOPLEFT, HTTOP, HTTOPRIGHT, HTRIGHT, HTBOTTOMRIGHT, HTBOTTOM, HTBOTTOMLEFT, HTLEFT
        self.system_drag([13, 12, 14, 11, 17, 15, 16, 10][edge as usize % 8]);
    }

    fn minimize(&mut self) {
        // SAFETY: our window.
        unsafe { ShowWindow(self.main, SW_MINIMIZE) };
    }

    fn toggle_maximize(&mut self) {
        // SAFETY: our window.
        unsafe { ShowWindow(self.main, if IsZoomed(self.main) != 0 { SW_RESTORE } else { SW_MAXIMIZE }) };
    }

    fn copy(&mut self, text: &str, file: bool) -> bool {
        if !open_clipboard(self.main) {
            return false;
        }
        let mut ok = true;
        // SAFETY: the clipboard is open; it owns the blocks it accepts.
        unsafe {
            EmptyClipboard();
            let units = text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            ok &= global_bytes(&utf16_bytes(units)).is_some_and(|mem| !SetClipboardData(CF_UNICODETEXT, mem).is_null());
            if file {
                // CF_HDROP: a DROPFILES header, then NUL-separated wide paths
                // ending in an extra NUL.
                let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, pt: POINT::default(), fNC: 0, fWide: 1 };
                let mut data =
                    std::slice::from_raw_parts((&header as *const DROPFILES).cast::<u8>(), size_of::<DROPFILES>()).to_vec();
                for line in text.lines().filter(|l| !l.is_empty()) {
                    data.extend(utf16_bytes(wide(line)));
                }
                data.extend([0, 0]);
                ok &= global_bytes(&data).is_some_and(|mem| !SetClipboardData(CF_HDROP, mem).is_null());
                // Tell Explorer that pasting copies the files.
                let effect = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
                if let Some(mem) = global_bytes(&DROPEFFECT_COPY.to_le_bytes()) {
                    SetClipboardData(effect, mem);
                }
            }
            CloseClipboard();
        }
        ok
    }

    fn request_paste(&mut self) {
        if let Some(text) = clipboard_text(self.main) {
            push(Event::Paste(text));
        }
    }

    fn keep_clipboard(&mut self) {
        // Windows keeps clipboard data after the program exits.
    }

    fn open_dialog(&mut self, w: i32, h: i32, title: &str) -> io::Result<()> {
        self.close_dialog();
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        // SAFETY (whole block): plain Win32 calls on our windows.
        unsafe {
            let dpi = GetDpiForWindow(self.main).max(96);
            let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
            AdjustWindowRectExForDpi(&mut r, style, 0, WS_EX_DLGMODALFRAME, dpi);
            let (ow, oh) = (r.right - r.left, r.bottom - r.top);
            // Centred over the main window.
            let mut m = RECT::default();
            GetWindowRect(self.main, &mut m);
            let (x, y) = (m.left + (m.right - m.left - ow) / 2, m.top + (m.bottom - m.top - oh) / 3);
            let title = wide(title);
            let dialog = CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                self.class.as_ptr(),
                title.as_ptr(),
                style,
                x,
                y.max(m.top),
                ow,
                oh,
                self.main, // owned: stays above it, minimized with it
                std::ptr::null_mut(),
                self.instance,
                std::ptr::null_mut(),
            );
            if dialog.is_null() {
                return Err(io::Error::last_os_error());
            }
            self.dialog = dialog;
            with(|s| {
                s.dialog = dialog as usize;
                s.frames[1] = None;
            });
            // Modal: the main window takes no input while it is open.
            EnableWindow(self.main, 0);
            match_title_bar(dialog);
            ShowWindow(dialog, SW_SHOW);
            SetForegroundWindow(dialog);
        }
        Ok(())
    }

    fn close_dialog(&mut self) {
        if self.dialog.is_null() {
            return;
        }
        // SAFETY: our windows. Re-enabling first lets activation return
        // to the main window rather than to another program.
        unsafe {
            EnableWindow(self.main, 1);
            ShowWindow(self.dialog, SW_HIDE);
            DestroyWindow(self.dialog);
        }
        self.dialog = std::ptr::null_mut();
        with(|s| {
            s.dialog = 0;
            s.frames[1] = None;
        });
    }

    fn start_drag(&mut self, paths: &str, _image: &Canvas, copy: bool) -> io::Result<()> {
        // The shell draws the drag image itself, as for a drag out of Explorer.
        let list: Vec<&Path> = paths.lines().filter(|l| !l.is_empty()).map(Path::new).collect();
        let Some(object) = files_data_object(&list) else {
            push(Event::DragEnded);
            return Ok(());
        };
        // Holding Ctrl over the target is handled by Windows itself.
        let allowed = if copy { DROPEFFECT_COPY } else { DROPEFFECT_COPY | DROPEFFECT_MOVE };
        let mut effect = 0u32;
        // SAFETY: our window and a live data object; the loop runs until
        // the button is released. A null source uses the default one.
        let hr = unsafe {
            ReleaseCapture();
            SHDoDragDrop(self.main, object, std::ptr::null_mut(), allowed, &mut effect)
        };
        release(object);
        if hr == DRAGDROP_S_DROP {
            // Explorer moves files on the same drive itself and may report
            // no effect: what is gone from disk tells.
            let moved = list.first().is_some_and(|p| !p.exists());
            push(Event::DragDropped { moves: moved || effect == DROPEFFECT_MOVE });
            push(Event::DragFinished);
        }
        push(Event::DragEnded);
        push(Event::Release { win: Win::Main, button: 1 });
        Ok(())
    }

    fn drag_wants_copy(&mut self, _copy: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_become_a_shell_data_object_with_their_paths() {
        // SAFETY: COM on this thread, for the shell objects.
        unsafe { OleInitialize(std::ptr::null_mut()) };
        let dir = std::env::temp_dir().join(format!("ff_gui_dnd_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (a, b) = (dir.join("one.txt"), dir.join("sub").join("two.txt"));
        std::fs::write(&a, "1").unwrap();
        std::fs::write(&b, "2").unwrap();
        let object = files_data_object(&[&a, &b]).expect("data object");

        #[repr(C)]
        struct FORMATETC {
            cf: u16,
            ptd: *mut c_void,
            aspect: u32,
            index: i32,
            tymed: u32,
        }
        #[repr(C)]
        struct STGMEDIUM {
            tymed: u32,
            handle: HANDLE,
            release: *mut c_void,
        }
        let format = FORMATETC { cf: CF_HDROP as u16, ptd: std::ptr::null_mut(), aspect: 1, index: -1, tymed: 1 };
        let mut medium = STGMEDIUM { tymed: 0, handle: std::ptr::null_mut(), release: std::ptr::null_mut() };
        // SAFETY: IDataObject::GetData is the fourth vtable entry.
        let hr = unsafe {
            let vtable = *(object as *const *const usize);
            let get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> i32 =
                std::mem::transmute(*vtable.add(3));
            get_data(object, &format, &mut medium)
        };
        assert!(hr >= 0, "GetData failed: {hr:#x}");
        let count = unsafe { DragQueryFileW(medium.handle, u32::MAX, std::ptr::null_mut(), 0) };
        let mut found = Vec::new();
        for i in 0..count {
            let mut buf = [0u16; 512];
            let len = unsafe { DragQueryFileW(medium.handle, i, buf.as_mut_ptr(), 512) } as usize;
            found.push(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        release(object);
        let canon = |p: &Path| ff_core::os::canonicalize(p).unwrap();
        assert_eq!(found.iter().map(|p| canon(p)).collect::<Vec<_>>(), [canon(&a), canon(&b)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keys_without_text_map_to_keysyms() {
        assert_eq!(special_key(0x09, false, true), Some(K_BACK_TAB));
        assert_eq!(special_key(0x0D, true, false), Some(K_KP_ENTER));
        assert_eq!(special_key(0x73, false, false), Some(K_F4));
        assert_eq!(special_key(0x41, false, false), None);
    }
}
