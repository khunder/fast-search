//! `ff -i`: search interactively in the terminal.
//!
//! A full-screen interface drawn with plain ANSI escape codes, with the
//! terminal in raw mode. No curses library is involved, so `ff` stays one
//! static binary. It draws on `/dev/tty` rather than standard output, so
//! the picked paths can be captured: `vim "$(ff -i)"`.

use std::fs::File;
use std::io::{self, Write};
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime};

use ff_core::bookmarks::Bookmark;
use ff_core::sizes::{Tree, human_size};
use ff_core::{CaseMode, Index, Kind, Query, QueryOptions, SearchOptions, Sort, group_digits};

/// How the session starts, from the command line.
pub struct Start {
    pub query: String,
    pub kind: Kind,
    pub case: CaseMode,
    pub regex: bool,
    pub word: bool,
    pub match_path: bool,
    pub wildcards_anywhere: bool,
    pub sort: Sort,
    pub reverse: bool,
    /// Separate the printed paths with NUL instead of a line break.
    pub null: bool,
}

/// Runs the interface; prints the picked paths once the screen is back.
pub fn run(db: PathBuf, index: Index, start: Start) -> Result<ExitCode, String> {
    let term = Term::open().map_err(|e| e.to_string())?;
    let stamp = std::fs::metadata(&db).and_then(|m| m.modified()).ok();
    let mut ui = Ui::new(term, db, stamp, index, start);
    let end = ui.main_loop();
    let null = ui.null;
    drop(ui); // gives the terminal back before anything is printed
    match end.map_err(|e| e.to_string())? {
        End::Picked(paths) => {
            let mut out = io::stdout().lock();
            for path in paths {
                let _ = out.write_all(&path);
                let _ = out.write_all(if null { b"\0" } else { b"\n" });
            }
            let _ = out.flush();
            Ok(ExitCode::SUCCESS)
        }
        End::Cancelled => Ok(ExitCode::from(1)),
        End::Interrupted => Ok(ExitCode::from(130)),
    }
}

// ---- the terminal -----------------------------------------------------------

#[cfg(unix)]
mod sys {
    use std::ffi::{c_int, c_short, c_ulong};

    /// Room for any C library's `struct termios`, which is only ever
    /// filled and read back by the C library itself.
    #[repr(C, align(8))]
    pub struct Termios(pub [u8; 128]);

    #[repr(C)]
    pub struct PollFd {
        pub fd: c_int,
        pub events: c_short,
        pub revents: c_short,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct WinSize {
        pub rows: u16,
        pub cols: u16,
        pub x: u16,
        pub y: u16,
    }

    unsafe extern "C" {
        pub fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
        pub fn tcsetattr(fd: c_int, when: c_int, t: *const Termios) -> c_int;
        pub fn cfmakeraw(t: *mut Termios);
        pub fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        pub fn kill(pid: c_int, signal: c_int) -> c_int;
        pub fn getpid() -> c_int;
        pub fn signal(signal: c_int, handler: usize) -> usize;
    }

    pub const SIGINT: c_int = 2;
    pub const SIGQUIT: c_int = 3;
    pub const SIG_DFL: usize = 0;
    pub const SIG_IGN: usize = 1;

    pub const TCSAFLUSH: c_int = 2;
    pub const TIOCGWINSZ: c_ulong = 0x5413;
    pub const POLLIN: c_short = 1;
    pub const SIGTSTP: c_int = 20;
}

/// The Windows console, driven with the same escape sequences: Windows 10
/// and later understand them once "virtual terminal" mode is on, in both
/// directions (keys and mouse clicks then arrive as escape sequences too).
#[cfg(windows)]
mod sys {
    #![allow(non_snake_case)]
    use std::ffi::c_void;

    pub type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Default)]
    pub struct Coord {
        pub x: i16,
        pub y: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct SmallRect {
        pub left: i16,
        pub top: i16,
        pub right: i16,
        pub bottom: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ScreenBufferInfo {
        pub size: Coord,
        pub cursor: Coord,
        pub attributes: u16,
        pub window: SmallRect,
        pub max_size: Coord,
    }

    /// An `INPUT_RECORD`, with the event read as a key event (the only
    /// kind whose fields we look at).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct InputRecord {
        pub event_type: u16,
        pub _pad: u16,
        pub key_down: i32,
        pub repeat: u16,
        pub vk: u16,
        pub scan: u16,
        pub ch: u16,
        pub control: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: Handle, mode: u32) -> i32;
        pub fn GetConsoleOutputCP() -> u32;
        pub fn SetConsoleOutputCP(cp: u32) -> i32;
        pub fn GetConsoleScreenBufferInfo(h: Handle, info: *mut ScreenBufferInfo) -> i32;
        pub fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        pub fn PeekConsoleInputW(h: Handle, buf: *mut InputRecord, len: u32, read: *mut u32) -> i32;
        pub fn ReadConsoleInputW(h: Handle, buf: *mut InputRecord, len: u32, read: *mut u32) -> i32;
        pub fn ReadConsoleW(h: Handle, buf: *mut u16, len: u32, read: *mut u32, control: *const c_void) -> i32;
        pub fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    pub const ENABLE_PROCESSED_INPUT: u32 = 0x1;
    pub const ENABLE_LINE_INPUT: u32 = 0x2;
    pub const ENABLE_ECHO_INPUT: u32 = 0x4;
    pub const ENABLE_WINDOW_INPUT: u32 = 0x8;
    pub const ENABLE_MOUSE_INPUT: u32 = 0x10;
    pub const ENABLE_QUICK_EDIT_MODE: u32 = 0x40;
    pub const ENABLE_EXTENDED_FLAGS: u32 = 0x80;
    pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x200;
    pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x1;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
    pub const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x8;
    pub const KEY_EVENT: u16 = 1;
    pub const WAIT_OBJECT_0: u32 = 0;
    pub const CP_UTF8: u32 = 65001;

    /// A console control handler that swallows Ctrl+C: while an editor
    /// runs in the console, Ctrl+C is meant for it, not for us.
    pub unsafe extern "system" fn ignore_ctrl_c(_: u32) -> i32 {
        1
    }
}

struct Term {
    /// Where the screen is drawn: `/dev/tty`, or the Windows console's
    /// output (`CONOUT$`), so standard output stays free for the result.
    tty: File,
    /// The console's input (`CONIN$`); on Unix `tty` serves both ways.
    #[cfg(windows)]
    conin: File,
    /// The settings to put back when we leave.
    #[cfg(unix)]
    saved: sys::Termios,
    /// Input mode, output mode and output code page to put back.
    #[cfg(windows)]
    saved: (u32, u32, u32),
    /// The first half of a UTF-16 surrogate pair, until the second comes.
    #[cfg(windows)]
    high_surrogate: Option<u16>,
    /// Bytes read but not yet turned into keys.
    input: Vec<u8>,
    /// The bytes of the last key we could not make sense of.
    unknown: Vec<u8>,
    active: bool,
}

#[cfg(unix)]
impl Term {
    fn open() -> io::Result<Term> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| io::Error::other("ff -i needs a terminal to draw on"))?;
        let mut saved = sys::Termios([0; 128]);
        // SAFETY: the buffer is larger than the C library's termios.
        if unsafe { sys::tcgetattr(tty.as_raw_fd(), &mut saved) } != 0 {
            return Err(io::Error::other("ff -i needs a terminal to draw on"));
        }
        let mut term = Term { tty, saved, input: Vec::new(), unknown: Vec::new(), active: false };
        term.enter()?;
        Ok(term)
    }

    /// Raw keys, our own screen, mouse clicks and the wheel.
    fn enter(&mut self) -> io::Result<()> {
        let mut raw = sys::Termios(self.saved.0);
        // SAFETY: both point to valid termios buffers.
        unsafe {
            sys::cfmakeraw(&mut raw);
            sys::tcsetattr(self.tty.as_raw_fd(), sys::TCSAFLUSH, &raw);
        }
        // Alternate screen, mouse buttons in SGR form, no wrap at the edge.
        self.tty.write_all(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?7l")?;
        self.active = true;
        Ok(())
    }

    fn leave(&mut self) {
        if std::mem::take(&mut self.active) {
            let _ = self.tty.write_all(b"\x1b[?7h\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
            // SAFETY: `saved` was filled by tcgetattr.
            unsafe { sys::tcsetattr(self.tty.as_raw_fd(), sys::TCSAFLUSH, &self.saved) };
        }
    }

    /// (rows, columns).
    fn size(&self) -> (usize, usize) {
        let mut ws = sys::WinSize::default();
        // SAFETY: TIOCGWINSZ fills a struct winsize.
        let ok = unsafe { sys::ioctl(self.tty.as_raw_fd(), sys::TIOCGWINSZ, &mut ws as *mut sys::WinSize) } == 0;
        if ok && ws.rows > 0 && ws.cols > 0 { (ws.rows as usize, ws.cols as usize) } else { (24, 80) }
    }

    /// Reads what has arrived, waiting at most `wait`; false if nothing did.
    fn fill(&mut self, wait: Duration) -> io::Result<bool> {
        let mut fd = sys::PollFd { fd: self.tty.as_raw_fd(), events: sys::POLLIN, revents: 0 };
        let ms = wait.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        if unsafe { sys::poll(&mut fd, 1, ms) } <= 0 {
            return Ok(false); // timed out, or a signal (a resize) came
        }
        let mut buf = [0u8; 4096];
        let n = self.tty.read(&mut buf)?;
        self.input.extend_from_slice(&buf[..n]);
        Ok(n > 0)
    }

    /// Standard handles for a program that takes over the terminal.
    fn child_stdio(&self) -> io::Result<(Stdio, Stdio, Stdio)> {
        let tty = |t: &File| t.try_clone().map(Stdio::from);
        Ok((tty(&self.tty)?, tty(&self.tty)?, tty(&self.tty)?))
    }
}

#[cfg(windows)]
impl Term {
    fn open() -> io::Result<Term> {
        let no_console = |_| io::Error::other("ff -i needs a console to draw on");
        let console = |name| std::fs::OpenOptions::new().read(true).write(true).open(name).map_err(no_console);
        let tty = console("CONOUT$")?;
        let conin = console("CONIN$")?;
        let (mut input_mode, mut output_mode) = (0, 0);
        // SAFETY: valid console handles and out-pointers.
        let ok = unsafe {
            sys::GetConsoleMode(conin.as_raw_handle(), &mut input_mode) != 0
                && sys::GetConsoleMode(tty.as_raw_handle(), &mut output_mode) != 0
        };
        if !ok {
            return Err(io::Error::other("ff -i needs a console to draw on"));
        }
        // SAFETY: no arguments.
        let code_page = unsafe { sys::GetConsoleOutputCP() };
        let saved = (input_mode, output_mode, code_page);
        let mut term = Term { tty, conin, saved, high_surrogate: None, input: Vec::new(), unknown: Vec::new(), active: false };
        term.enter()?;
        Ok(term)
    }

    /// Raw keys as escape sequences, our own screen, mouse clicks and the
    /// wheel, UTF-8 output.
    fn enter(&mut self) -> io::Result<()> {
        use sys::*;
        let input = (self.saved.0
            & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE))
            | ENABLE_VIRTUAL_TERMINAL_INPUT
            | ENABLE_WINDOW_INPUT
            | ENABLE_EXTENDED_FLAGS;
        let output = self.saved.1 | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        // SAFETY: valid console handles.
        let ok = unsafe {
            SetConsoleMode(self.conin.as_raw_handle(), input) != 0
                && SetConsoleMode(self.tty.as_raw_handle(), output) != 0
                && SetConsoleOutputCP(CP_UTF8) != 0
        };
        if !ok {
            self.restore();
            return Err(io::Error::other("this console does not understand escape sequences (Windows 10 or later is needed)"));
        }
        self.tty.write_all(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?7l")?;
        self.active = true;
        Ok(())
    }

    fn restore(&mut self) {
        // SAFETY: valid console handles; the values were read at start.
        unsafe {
            sys::SetConsoleMode(self.conin.as_raw_handle(), self.saved.0);
            sys::SetConsoleMode(self.tty.as_raw_handle(), self.saved.1);
            sys::SetConsoleOutputCP(self.saved.2);
        }
    }

    fn leave(&mut self) {
        if std::mem::take(&mut self.active) {
            let _ = self.tty.write_all(b"\x1b[?7h\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
            self.restore();
        }
    }

    /// (rows, columns) of the visible window.
    fn size(&self) -> (usize, usize) {
        let mut info = sys::ScreenBufferInfo::default();
        // SAFETY: a valid console handle and out-pointer.
        let ok = unsafe { sys::GetConsoleScreenBufferInfo(self.tty.as_raw_handle(), &mut info) } != 0;
        let rows = (info.window.bottom - info.window.top + 1) as usize;
        let cols = (info.window.right - info.window.left + 1) as usize;
        if ok && rows > 0 && cols > 0 { (rows, cols) } else { (24, 80) }
    }

    /// Reads what has arrived, waiting at most `wait`; false if nothing did.
    fn fill(&mut self, wait: Duration) -> io::Result<bool> {
        let h = self.conin.as_raw_handle();
        let ms = wait.as_millis().min(u32::MAX as u128 - 1) as u32;
        // SAFETY: a valid console input handle.
        if unsafe { sys::WaitForSingleObject(h, ms) } != sys::WAIT_OBJECT_0 {
            return Ok(false);
        }
        // Something is waiting, but maybe only focus or resize events,
        // which reading text would block on: those are taken off here.
        let mut records = [sys::InputRecord::default(); 64];
        let mut n = 0;
        // SAFETY: the buffer holds `records.len()` records.
        if unsafe { sys::PeekConsoleInputW(h, records.as_mut_ptr(), records.len() as u32, &mut n) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let typed = records[..n as usize].iter().any(|r| r.event_type == sys::KEY_EVENT && r.key_down != 0 && r.ch != 0);
        if !typed {
            // SAFETY: as above; reads exactly the records just seen.
            unsafe { sys::ReadConsoleInputW(h, records.as_mut_ptr(), n, &mut n) };
            return Ok(false); // a resize: the next draw picks up the size
        }
        let mut buf = [0u16; 2048];
        let mut got = 0;
        // SAFETY: the buffer holds `buf.len()` UTF-16 units.
        if unsafe { sys::ReadConsoleW(h, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut units: Vec<u16> = self.high_surrogate.take().into_iter().collect();
        units.extend_from_slice(&buf[..got as usize]);
        if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            self.high_surrogate = units.pop();
        }
        self.input.extend_from_slice(String::from_utf16_lossy(&units).as_bytes());
        Ok(!units.is_empty())
    }

    /// Standard handles for a program that takes over the console.
    fn child_stdio(&self) -> io::Result<(Stdio, Stdio, Stdio)> {
        let clone = |t: &File| t.try_clone().map(Stdio::from);
        Ok((clone(&self.conin)?, clone(&self.tty)?, clone(&self.tty)?))
    }
}

impl Term {
    /// The next key, waiting at most `wait` for one.
    fn next_key(&mut self, wait: Duration) -> io::Result<Option<Key>> {
        loop {
            if !self.input.is_empty() {
                match parse_key(&self.input) {
                    Some((key, used)) => {
                        let bytes: Vec<u8> = self.input.drain(..used).collect();
                        if key == Key::Unknown {
                            self.unknown = bytes;
                        }
                        return Ok(Some(key));
                    }
                    // The start of a longer sequence: give the rest a moment.
                    None if self.fill(Duration::from_millis(30))? => continue,
                    None => {
                        let lone_escape = self.input[0] == 0x1b;
                        self.input.drain(..1);
                        return Ok(Some(if lone_escape { Key::Esc } else { Key::Unknown }));
                    }
                }
            }
            if !self.fill(wait)? {
                return Ok(None);
            }
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.leave();
    }
}

// ---- keys -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Char(char),
    /// Ctrl and a letter, as the lowercase letter.
    Ctrl(char),
    Alt(char),
    CtrlSpace,
    Enter,
    AltEnter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    AltDown,
    Left,
    Right,
    CtrlLeft,
    CtrlRight,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    /// A mouse button (0 left, 64 and 65 the wheel) at a cell, 0-based.
    Mouse { button: u16, x: usize, y: usize, press: bool },
    Unknown,
}

/// The key at the start of `b` and how many bytes it took, or `None` when
/// `b` holds only the beginning of one.
fn parse_key(b: &[u8]) -> Option<(Key, usize)> {
    let first = *b.first()?;
    match first {
        0x1b => parse_escape(b),
        b'\r' | b'\n' => Some((Key::Enter, 1)),
        b'\t' => Some((Key::Tab, 1)),
        0x7f | 0x08 => Some((Key::Backspace, 1)),
        0 => Some((Key::CtrlSpace, 1)),
        1..=26 => Some((Key::Ctrl((b'a' + first - 1) as char), 1)),
        0x1c..=0x1f => Some((Key::Unknown, 1)),
        _ => {
            let (ch, len) = decode(b)?;
            Some((ch.map_or(Key::Unknown, Key::Char), len))
        }
    }
}

/// One UTF-8 character: `None` if incomplete, `Some((None, n))` if bad.
fn decode(b: &[u8]) -> Option<(Option<char>, usize)> {
    let len = match b[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Some((None, 1)),
    };
    if b.len() < len {
        return None;
    }
    let ch = std::str::from_utf8(&b[..len]).ok().and_then(|s| s.chars().next());
    Some((ch, if ch.is_some() { len } else { 1 }))
}

fn parse_escape(b: &[u8]) -> Option<(Key, usize)> {
    match *b.get(1)? {
        b'[' => {
            // CSI: parameters, then a final byte.
            let end = b.iter().skip(2).position(|c| (0x40..=0x7e).contains(c));
            let Some(end) = end.map(|i| i + 2) else {
                return if b.len() > 32 { Some((Key::Unknown, b.len())) } else { None };
            };
            let params = std::str::from_utf8(&b[2..end]).unwrap_or("");
            Some((csi(params, b[end]), end + 1))
        }
        b'O' => {
            let key = match *b.get(2)? {
                b'P' => Key::F(1),
                b'Q' => Key::F(2),
                b'R' => Key::F(3),
                b'S' => Key::F(4),
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                b'M' => Key::Enter,
                _ => Key::Unknown,
            };
            Some((key, 3))
        }
        b'\r' | b'\n' => Some((Key::AltEnter, 2)),
        0x1b => Some((Key::Esc, 1)),
        _ => {
            let (ch, len) = decode(&b[1..])?;
            Some((ch.map_or(Key::Unknown, Key::Alt), len + 1))
        }
    }
}

fn csi(params: &str, last: u8) -> Key {
    if let Some(mouse) = params.strip_prefix('<') {
        let n: Vec<usize> = mouse.split(';').map(|v| v.parse().unwrap_or(0)).collect();
        if n.len() == 3 {
            let (x, y) = (n[1].saturating_sub(1), n[2].saturating_sub(1));
            return Key::Mouse { button: n[0] as u16 & !(4 | 8 | 16), x, y, press: last == b'M' };
        }
        return Key::Unknown;
    }
    let mut numbers = params.split(';').map(|v| v.parse::<u32>().unwrap_or(1));
    let first = numbers.next().unwrap_or(1);
    // Modifiers: 2 Shift, 3 Alt, 5 Ctrl (one more than the bits).
    let modifiers = numbers.next().unwrap_or(1);
    let (alt, ctrl) = (modifiers == 3, modifiers == 5);
    match last {
        b'A' => Key::Up,
        b'B' if alt => Key::AltDown,
        b'B' => Key::Down,
        b'C' if ctrl => Key::CtrlRight,
        b'C' => Key::Right,
        b'D' if ctrl => Key::CtrlLeft,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::BackTab,
        b'P' => Key::F(1),
        b'Q' => Key::F(2),
        b'R' => Key::F(3),
        b'S' => Key::F(4),
        b'~' => match first {
            1 | 7 => Key::Home,
            4 | 8 => Key::End,
            3 => Key::Delete,
            5 => Key::PageUp,
            6 => Key::PageDown,
            11..=15 => Key::F((first - 10) as u8),
            17..=21 => Key::F((first - 11) as u8),
            23 | 24 => Key::F((first - 12) as u8),
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}

// ---- text on a grid of cells ----------------------------------------------------

/// Cells a character takes: 2 for wide East Asian characters and emoji,
/// 0 for combining marks.
fn cells(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6 => 2,
        0x1f300..=0x1f64f | 0x1f900..=0x1f9ff | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

fn width(s: &str) -> usize {
    s.chars().map(cells).sum()
}

/// A name as it may be shown: control characters (a file name may hold
/// escape codes or line breaks) become "?", so they cannot drive the
/// terminal.
fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// `s` cut to `w` cells, ending in "…" when cut.
fn fit(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        if used + cells(c) + 1 > w {
            break;
        }
        used += cells(c);
        out.push(c);
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// `s` cut to `w` cells from the left, starting with "…" when cut: the
/// end of a path says more than its start.
fn fit_end(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        if used + cells(c) + 1 > w {
            break;
        }
        used += cells(c);
        kept.push(c);
    }
    let mut out = String::from(if w > 0 { "…" } else { "" });
    out.extend(kept.into_iter().rev());
    out
}

fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// A bar of `w` cells filled to `fraction`, with eighths at its end.
fn bar(fraction: f64, w: usize) -> String {
    let eighths = (fraction.clamp(0.0, 1.0) * (w * 8) as f64).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        out.push([' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8]);
    }
    pad(&out, w)
}

// ---- the interface --------------------------------------------------------------

enum End {
    Picked(Vec<Vec<u8>>),
    Cancelled,
    Interrupted,
}

/// Colours, or none at all when NO_COLOR is set.
struct Paint {
    folder: &'static str,
    link: &'static str,
    dim: &'static str,
    accent: &'static str,
    error: &'static str,
}

struct Complete {
    items: Vec<(String, &'static str)>,
    chosen: usize,
    /// Where the word being completed starts in the search text.
    start: usize,
}

/// Ways of sorting that Ctrl+S steps through: the order, and whether it
/// runs backwards (sizes and dates largest and newest first).
const SORTS: [(Sort, bool, &str); 4] = [(Sort::Name, false, "name"), (Sort::Size, true, "size, largest first"), (Sort::Mtime, true, "date, newest first"), (Sort::Path, false, "path")];

const HELP: &[(&str, &str)] = &[
    ("type", "search as you type, with the same syntax as ff"),
    ("Up, Down, PgUp, PgDn", "move through the results (also Ctrl+P, Ctrl+N, the wheel)"),
    ("Enter", "open in your editor; when the output is captured, as in"),
    ("", "vim \"$(ff -i)\", print the path and quit instead"),
    ("Alt+P", "print the selected paths and quit"),
    ("Tab, Shift+Tab", "mark several results; Enter and the keys below act on all of them"),
    ("Ctrl+O", "open with the default application"),
    (
        "F4, Alt+E",
        if cfg!(windows) {
            "edit in %VISUAL% or %EDITOR% (else Notepad)"
        } else {
            "edit in $VISUAL or $EDITOR (else vi), in this terminal"
        },
    ),
    ("Alt+S", "edit with sudoedit; Enter and F4 do that for files you cannot change"),
    ("", "(if sudo is refused, the file opens read-only)"),
    ("Ctrl+G", "show in the file manager"),
    ("Alt+Enter", "properties"),
    ("Ctrl+Y", "copy the path (to your clipboard, if the terminal allows it)"),
    ("Alt+Down, Ctrl+Space", "complete a filter: ext:  size:  modified:"),
    ("Alt+1 to Alt+9", "run a bookmark saved in the window"),
    ("F2", "show everything, files only or folders only"),
    ("F3", "letter case: smart, sensitive, insensitive"),
    ("Ctrl+R, Ctrl+B", "regular expressions, whole words"),
    ("Ctrl+S", "sort by name, size, date or path"),
    ("F6", "folder sizes: Right and Left open and close, Esc goes back"),
    ("Left, Right, Home, End", "move in the search text (Ctrl+A, Ctrl+E too)"),
    ("Ctrl+W, Ctrl+U, Ctrl+K", "delete a word, everything before, everything after"),
    ("Esc", "clear the search; quit when it is empty"),
    ("Ctrl+C, Ctrl+Q", "quit without printing anything"),
];

struct Ui {
    term: Term,
    db: PathBuf,
    stamp: Option<SystemTime>,
    index: Index,
    query: String,
    cursor: usize,
    hits: Vec<u32>,
    sel: usize,
    top: usize,
    /// Marked results, in the order they were marked.
    marks: Vec<u32>,
    kind: Kind,
    case: CaseMode,
    regex: bool,
    word: bool,
    match_path: bool,
    wildcards_anywhere: bool,
    sort: usize,
    /// The search must run again before the next frame.
    stale: bool,
    error: Option<String>,
    took: Duration,
    note: Option<(String, Instant)>,
    complete: Option<Complete>,
    tree: Option<Tree>,
    /// A panel over the list (help, properties): its title and lines.
    panel: Option<(String, Vec<(String, String)>)>,
    bookmarks: Vec<Bookmark>,
    last_click: (Instant, usize),
    last_check: Instant,
    size: (usize, usize),
    paint: Paint,
    null: bool,
    /// Our output goes to a pipe or a file, not the screen: Enter prints
    /// the choice for the command that reads it.
    capture: bool,
}

impl Ui {
    fn new(term: Term, db: PathBuf, stamp: Option<SystemTime>, index: Index, start: Start) -> Ui {
        let plain = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let paint = if plain {
            Paint { folder: "", link: "", dim: "", accent: "", error: "" }
        } else {
            Paint { folder: "\x1b[1;34m", link: "\x1b[36m", dim: "\x1b[2m", accent: "\x1b[1;33m", error: "\x1b[1;31m" }
        };
        let sort = SORTS.iter().position(|(s, r, _)| *s == start.sort && *r == start.reverse).unwrap_or(0);
        let cursor = start.query.len();
        let size = term.size();
        Ui {
            term,
            db,
            stamp,
            index,
            query: start.query,
            cursor,
            hits: Vec::new(),
            sel: 0,
            top: 0,
            marks: Vec::new(),
            kind: start.kind,
            case: start.case,
            regex: start.regex,
            word: start.word,
            match_path: start.match_path,
            wildcards_anywhere: start.wildcards_anywhere,
            sort,
            stale: true,
            error: None,
            took: Duration::ZERO,
            note: None,
            complete: None,
            tree: None,
            panel: None,
            bookmarks: ff_core::bookmarks::load(),
            last_click: (Instant::now(), usize::MAX),
            last_check: Instant::now(),
            size,
            paint,
            null: start.null,
            // Standard output captured, as in `vim "$(ff -i)"`.
            capture: !std::io::IsTerminal::is_terminal(&io::stdout()),
        }
    }

    fn main_loop(&mut self) -> io::Result<End> {
        loop {
            if self.stale {
                self.search();
            }
            self.draw()?;
            let Some(key) = self.term.next_key(Duration::from_millis(500))? else {
                self.idle();
                continue;
            };
            if let Some(end) = self.key(key)? {
                return Ok(end);
            }
            // Keys already typed (a paste, fast typing) go in before the
            // next frame, so the search runs once for all of them.
            while let Some(key) = self.term.next_key(Duration::ZERO)? {
                if let Some(end) = self.key(key)? {
                    return Ok(end);
                }
            }
        }
    }

    /// Nothing was typed for a while: notes expire, and a newer index
    /// (rebuilt, or patched by the daemon) is loaded.
    fn idle(&mut self) {
        if self.note.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4)) {
            self.note = None;
        }
        if self.last_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_check = Instant::now();
        let stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
        if stamp == self.stamp {
            return;
        }
        let Ok(index) = Index::load(&self.db) else { return }; // mid-rewrite: next time
        self.stamp = stamp;
        let kept = self.tree.as_ref().map(|t| t.keep(&self.index));
        let selected = self.hits.get(self.sel).copied();
        self.index = index;
        self.tree = kept.map(|k| Tree::restored(&self.index, &k));
        self.marks.clear(); // their ids belong to the old index
        self.search();
        if let Some(row) = selected.and_then(|id| self.hits.iter().position(|&h| h == id)) {
            self.sel = row;
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), Instant::now()));
    }

    fn search(&mut self) {
        self.stale = false;
        let how = QueryOptions { case: self.case, regex: self.regex, whole_word: self.word, match_path: self.match_path, wildcards_anywhere: self.wildcards_anywhere };
        let query = Query::parse_opts(&self.query, &how);
        self.error = query.error().map(String::from);
        let (sort, reverse, _) = SORTS[self.sort];
        let started = Instant::now();
        self.hits = self.index.search(&query, &SearchOptions { kind: self.kind, sort, reverse, ..Default::default() });
        self.took = started.elapsed();
        (self.sel, self.top) = (0, 0);
    }

    fn path(&self, id: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        self.index.write_path(id, &mut buf);
        buf
    }

    /// What an action applies to: the selected entry of the folder sizes
    /// view, or the marked results, or the selected one.
    fn targets(&self) -> Vec<u32> {
        if let Some(tree) = &self.tree {
            return tree.selected().into_iter().collect();
        }
        if !self.marks.is_empty() {
            return self.marks.clone();
        }
        self.hits.get(self.sel).copied().into_iter().collect()
    }

    fn body_rows(&self) -> usize {
        self.size.0.saturating_sub(2).max(1)
    }

    // ---- keys -------------------------------------------------------------------

    fn key(&mut self, key: Key) -> io::Result<Option<End>> {
        match key {
            Key::Ctrl('c') => return Ok(Some(End::Interrupted)),
            Key::Ctrl('q') => return Ok(Some(End::Cancelled)),
            Key::Ctrl('z') => {
                self.suspend();
                return Ok(None);
            }
            _ => {}
        }
        // Any key closes a panel.
        if self.panel.take().is_some() && !matches!(key, Key::Mouse { press: false, .. }) {
            return Ok(None);
        }
        if key == Key::Unknown {
            let shown: Vec<String> = self.term.unknown.iter().map(|&b| match b {
                0x1b => "Esc".to_string(),
                0x20..=0x7e => (b as char).to_string(),
                _ => format!("\\x{b:02x}"),
            }).collect();
            self.note(format!("This key is not used here (it sent {})", shown.join(" ")));
            return Ok(None);
        }
        if self.complete.is_some() && self.completion_key(key) {
            return Ok(None);
        }
        let before = (self.query.clone(), self.cursor);
        let was_open = self.complete.is_some();
        let end = if self.tree.is_some() { self.tree_key(key)? } else { self.search_key(key)? };
        // An open completion list follows the typing; other keys close it.
        if was_open && self.complete.is_some() {
            if before != (self.query.clone(), self.cursor) && self.tree.is_none() {
                self.update_completion();
            } else {
                self.complete = None;
            }
        }
        Ok(end)
    }

    fn search_key(&mut self, key: Key) -> io::Result<Option<End>> {
        let page = self.body_rows();
        let last = self.hits.len().saturating_sub(1);
        match key {
            Key::Char(c) => self.insert(c.encode_utf8(&mut [0; 4])),
            Key::Enter => return self.enter(),
            Key::Esc if self.query.is_empty() => return Ok(Some(End::Cancelled)),
            Key::Esc | Key::Ctrl('u') if self.cursor == self.query.len() => self.edit(0, self.query.len(), ""),
            Key::Ctrl('u') => self.edit(0, self.cursor, ""),
            Key::Ctrl('k') => self.edit(self.cursor, self.query.len(), ""),
            Key::Ctrl('w') => self.edit(self.step(false, true), self.cursor, ""),
            Key::Backspace => self.edit(self.step(false, false), self.cursor, ""),
            Key::Delete => self.edit(self.cursor, self.step(true, false), ""),
            Key::Left => self.cursor = self.step(false, false),
            Key::Right => self.cursor = self.step(true, false),
            Key::CtrlLeft | Key::Alt('b') => self.cursor = self.step(false, true),
            Key::CtrlRight | Key::Alt('f') => self.cursor = self.step(true, true),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.query.len(),
            Key::Up | Key::Ctrl('p') => self.sel = self.sel.saturating_sub(1),
            Key::Down | Key::Ctrl('n') => self.sel = (self.sel + 1).min(last),
            Key::PageUp => self.sel = self.sel.saturating_sub(page),
            Key::PageDown => self.sel = (self.sel + page).min(last),
            Key::Tab | Key::BackTab => {
                if let Some(&id) = self.hits.get(self.sel) {
                    match self.marks.iter().position(|&m| m == id) {
                        Some(i) => drop(self.marks.remove(i)),
                        None => self.marks.push(id),
                    }
                    self.sel = if key == Key::Tab { (self.sel + 1).min(last) } else { self.sel.saturating_sub(1) };
                }
            }
            Key::AltDown | Key::CtrlSpace => {
                self.update_completion();
                if self.complete.is_none() {
                    self.note("Nothing to complete here: try an empty word, or after ext:  size:  modified:");
                }
            }
            Key::Alt(d @ '1'..='9') => self.run_bookmark(d as usize - '1' as usize),
            Key::F(2) => {
                self.kind = match self.kind {
                    Kind::Any => Kind::File,
                    Kind::File => Kind::Dir,
                    Kind::Dir => Kind::Any,
                };
                self.stale = true;
            }
            Key::F(3) => {
                self.case = match self.case {
                    CaseMode::Smart => CaseMode::Sensitive,
                    CaseMode::Sensitive => CaseMode::Insensitive,
                    CaseMode::Insensitive => CaseMode::Smart,
                };
                self.stale = true;
            }
            Key::Ctrl('r') => (self.regex, self.stale) = (!self.regex, true),
            Key::Ctrl('b') => (self.word, self.stale) = (!self.word, true),
            Key::Ctrl('s') => {
                self.sort = (self.sort + 1) % SORTS.len();
                self.note(format!("Sorted by {}", SORTS[self.sort].2));
                self.stale = true;
            }
            Key::F(6) => self.open_tree(),
            Key::F(1) => self.show_help(),
            Key::Mouse { button, x, y, press: true } => return self.search_mouse(button, x, y),
            _ => return self.action_key(key),
        }
        Ok(None)
    }

    /// The keys that act on files, the same in both views.
    fn action_key(&mut self, key: Key) -> io::Result<Option<End>> {
        let Some(&id) = self.targets().first() else { return Ok(None) };
        let path = PathBuf::from(ff_core::os::os_string_from_vec(self.path(id)));
        match key {
            Key::Ctrl('o') => match desktop() {
                true => self.done(ff_core::desktop::open(&path), &path, "Opened"),
                false => self.note("No desktop to open it on: Enter prints the path, F4 edits it here"),
            },
            Key::Ctrl('g') => match desktop() {
                true => self.done(ff_core::desktop::reveal(&path), &path, "Shown in the file manager:"),
                false => self.note("No desktop to show it on"),
            },
            Key::F(4) | Key::Alt('e') => {
                let paths = self.target_paths();
                self.edit_here(&paths, false)?;
            }
            Key::Alt('s') => {
                let paths = self.target_paths();
                self.edit_here(&paths, true)?;
            }
            // Print the paths and quit, even when the output is the screen.
            Key::Alt('p') => return Ok(self.pick()),
            Key::AltEnter => match ff_core::desktop::properties(&path) {
                Ok(props) => self.panel = Some(("Properties".into(), props.into_iter().map(|(k, v)| (k.to_string(), v)).collect())),
                Err(e) => self.note(format!("{}: {e}", path.display())),
            },
            Key::Ctrl('y') => {
                let paths: Vec<Vec<u8>> = self.targets().into_iter().map(|id| self.path(id)).collect();
                let text = paths.join(&b'\n');
                write!(self.term.tty, "\x1b]52;c;{}\x07", base64(&text))?;
                self.note(match paths.len() {
                    1 => "Copied the path (if your terminal lets programs use the clipboard)".to_string(),
                    n => format!("Copied {n} paths (if your terminal lets programs use the clipboard)"),
                });
            }
            _ => {}
        }
        Ok(None)
    }

    fn done(&mut self, result: io::Result<()>, path: &Path, what: &str) {
        match result {
            Ok(()) => self.note(format!("{what} {}", path.display())),
            Err(e) => self.note(e.to_string()),
        }
    }

    /// Enter: open the selection in the editor, or, when our output is
    /// captured (`vim "$(ff -i)"`), print it and quit.
    fn enter(&mut self) -> io::Result<Option<End>> {
        if self.capture {
            return Ok(self.pick());
        }
        let paths = self.target_paths();
        self.edit_here(&paths, false)?;
        Ok(None)
    }

    fn target_paths(&self) -> Vec<PathBuf> {
        self.targets().into_iter().map(|id| PathBuf::from(ff_core::os::os_string_from_vec(self.path(id)))).collect()
    }

    fn pick(&self) -> Option<End> {
        let paths: Vec<Vec<u8>> = self.targets().into_iter().map(|id| self.path(id)).collect();
        (!paths.is_empty()).then_some(End::Picked(paths))
    }

    fn search_mouse(&mut self, button: u16, _x: usize, y: usize) -> io::Result<Option<End>> {
        let last = self.hits.len().saturating_sub(1);
        match button {
            64 => self.sel = self.sel.saturating_sub(3),
            65 => self.sel = (self.sel + 3).min(last),
            0 if y >= 1 && y <= self.body_rows() => {
                let row = self.top + y - 1;
                if row < self.hits.len() {
                    let double = self.last_click.1 == row && self.last_click.0.elapsed() < Duration::from_millis(400);
                    self.sel = row;
                    self.last_click = (Instant::now(), row);
                    if double {
                        return self.enter();
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    // ---- the search text --------------------------------------------------------

    fn insert(&mut self, s: &str) {
        self.edit(self.cursor, self.cursor, s);
    }

    fn edit(&mut self, from: usize, to: usize, with: &str) {
        if from == to && with.is_empty() {
            return;
        }
        self.query.replace_range(from..to, with);
        self.cursor = from + with.len();
        self.stale = true;
    }

    /// The caret position one character (or word) to the left or right.
    fn step(&self, right: bool, word: bool) -> usize {
        let q = &self.query;
        if right {
            let rest = &q[self.cursor..];
            let mut it = rest.char_indices().peekable();
            if !word {
                return it.nth(1).map_or(q.len(), |(i, _)| self.cursor + i);
            }
            let start = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
            let end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |i| start + i);
            self.cursor + end
        } else {
            let before = &q[..self.cursor];
            if !word {
                return before.char_indices().last().map_or(0, |(i, _)| i);
            }
            let trimmed = before.trim_end();
            trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1)
        }
    }

    // ---- completion ---------------------------------------------------------------

    fn update_completion(&mut self) {
        let start = self.query[..self.cursor].rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let (y, m, d, ..) = ff_core::local_civil(now);
        let items = ff_core::complete::suggestions(&self.query[start..self.cursor], (y, m, d));
        let before = self.complete.as_ref().and_then(|c| c.items.get(c.chosen)).map(|(t, _)| t.clone());
        let chosen = before.and_then(|t| items.iter().position(|(i, _)| *i == t)).unwrap_or(0);
        self.complete = (!items.is_empty()).then_some(Complete { items, chosen, start });
    }

    /// A key while the list is open; false leaves it to the search box.
    fn completion_key(&mut self, key: Key) -> bool {
        let Some(c) = self.complete.as_mut() else { return false };
        let n = c.items.len();
        match key {
            Key::Up | Key::Ctrl('p') => c.chosen = (c.chosen + n - 1) % n,
            Key::Down | Key::Ctrl('n') | Key::AltDown => c.chosen = (c.chosen + 1) % n,
            Key::Esc => self.complete = None,
            Key::Enter | Key::Tab => {
                let Some(c) = self.complete.take() else { return true };
                let text = c.items[c.chosen].0.clone();
                let end = self.query[self.cursor..].find(char::is_whitespace).map_or(self.query.len(), |i| self.cursor + i);
                let names_filter = text.ends_with(':');
                let space = if names_filter || self.query[end..].starts_with(' ') { "" } else { " " };
                self.edit(c.start, end, &format!("{text}{space}"));
                if names_filter {
                    self.update_completion();
                }
            }
            _ => return false,
        }
        true
    }

    // ---- other places ---------------------------------------------------------------

    fn run_bookmark(&mut self, i: usize) {
        self.bookmarks = ff_core::bookmarks::load();
        let Some(b) = self.bookmarks.get(i).cloned() else {
            return self.note(format!("There is no bookmark {} (save them in the window with Ctrl+D)", i + 1));
        };
        self.regex = b.flags.contains('r');
        self.word = b.flags.contains('w');
        self.kind = if b.flags.contains('f') {
            Kind::File
        } else if b.flags.contains('d') {
            Kind::Dir
        } else {
            Kind::Any
        };
        self.query = b.query;
        self.cursor = self.query.len();
        self.stale = true;
        self.note(format!("Bookmark: {}", b.name));
    }

    fn show_help(&mut self) {
        let lines = HELP.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.panel = Some(("Keys  (any key closes this)".into(), lines));
    }

    /// Hands the terminal to the user's editor until it exits. Files the
    /// user may not change go through `sudoedit`, which asks for the
    /// password here, lets the editor work on copies as the user, and
    /// writes a copy back as root only if it was changed. If sudo is
    /// refused (wrong password, Ctrl+C), the files open read-only instead.
    fn edit_here(&mut self, paths: &[PathBuf], force_sudo: bool) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let editor = ["VISUAL", "EDITOR"].iter().find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty())).unwrap_or_else(|| DEFAULT_EDITOR.into());
        let all_files = paths.iter().all(|p| p.symlink_metadata().is_ok_and(|m| !m.is_dir()));
        // Windows has no sudoedit: a file the user may not change opens anyway.
        let sudo = cfg!(unix) && all_files && (force_sudo || paths.iter().any(|p| !may(p, 2)));
        let stamp = |p: &PathBuf| std::fs::metadata(p).ok().map(|m| (m.modified().ok(), m.len()));
        let before: Vec<_> = paths.iter().map(stamp).collect();
        let what = if paths.len() == 1 { paths[0].display().to_string() } else { format!("{} files", paths.len()) };

        let status = if sudo {
            // sudoedit runs SUDO_EDITOR as the user, never as root.
            let mut command = Command::new("sh");
            command.env("SUDO_EDITOR", &editor).args(["-c", "exec sudoedit -- \"$@\"", "sh"]).args(paths);
            self.in_terminal(command, &format!("ff: you cannot change {what} yourself: editing with sudoedit."))?
        } else {
            self.in_terminal(editor_command(&editor, paths, false), "")?
        };
        let changed: Vec<&PathBuf> = paths.iter().zip(&before).filter(|(p, b)| stamp(p) != **b).map(|(p, _)| p).collect();

        // Whether anything was written is read from the files themselves:
        // quitting with :q leaves them as they were.
        let refused = sudo && changed.is_empty() && !status.as_ref().is_ok_and(|s| s.success());
        if refused {
            if !paths.iter().all(|p| may(p, 4)) {
                self.note(format!("sudo was refused, and you may not read {what}"));
                return Ok(());
            }
            let viewer = editor_command(&editor, paths, true);
            if let Err(e) = self.in_terminal(viewer, &format!("ff: sudo was refused: opening {what} read-only.")) {
                self.note(format!("could not start {editor}: {e}"));
            } else {
                self.note(format!("Opened {what} read-only: sudo was refused"));
            }
            return Ok(());
        }
        match (status, changed.len()) {
            (Err(e), _) => self.note(format!("could not start {}: {e}", if sudo { "sudoedit" } else { &editor })),
            (_, 0) if sudo || paths.len() > 1 => self.note(if paths.len() == 1 { format!("{what} was not changed") } else { "No file was changed".into() }),
            (_, 0) => {}
            (_, 1) => self.note(format!("Saved {}", changed[0].display())),
            (_, n) => self.note(format!("Saved {n} files")),
        }
        Ok(())
    }

    /// Runs `command` on the terminal, with our screen put away, and
    /// shows `notice` above it first.
    fn in_terminal(&mut self, mut command: Command, notice: &str) -> io::Result<io::Result<std::process::ExitStatus>> {
        self.term.leave();
        if !notice.is_empty() {
            let _ = writeln!(self.term.tty, "{notice}\r");
        }
        let (stdin, stdout, stderr) = self.term.child_stdio()?;
        command.stdin(stdin).stdout(stdout).stderr(stderr);
        let status = run_ignoring_ctrl_c(command);
        self.term.enter()?;
        Ok(status)
    }

    /// Ctrl+Z: back to the shell; `fg` brings the screen back.
    #[cfg(unix)]
    fn suspend(&mut self) {
        self.term.leave();
        // SAFETY: stopping ourselves; execution resumes here on SIGCONT.
        unsafe { sys::kill(sys::getpid(), sys::SIGTSTP) };
        let _ = self.term.enter();
    }

    /// Windows consoles have no job control to go back to.
    #[cfg(windows)]
    fn suspend(&mut self) {
        self.note("Ctrl+Z does nothing here: Windows consoles cannot suspend a program".to_string());
    }

    // ---- folder sizes ---------------------------------------------------------------

    fn open_tree(&mut self) {
        let mut tree = Tree::new(&self.index);
        if let Some(&id) = self.hits.get(self.sel).filter(|_| !self.query.trim().is_empty()) {
            if tree.reveal(&self.index, id) {
                tree.open(tree.sel);
            }
        }
        self.tree = Some(tree);
    }

    fn tree_key(&mut self, key: Key) -> io::Result<Option<End>> {
        let page = self.body_rows();
        let Some(tree) = self.tree.as_mut() else { return Ok(None) };
        let last = tree.rows.len().saturating_sub(1);
        match key {
            Key::Esc | Key::F(6) => self.tree = None,
            Key::Up | Key::Ctrl('p') => tree.sel = tree.sel.saturating_sub(1),
            Key::Down | Key::Ctrl('n') => tree.sel = (tree.sel + 1).min(last),
            Key::PageUp => tree.sel = tree.sel.saturating_sub(page),
            Key::PageDown => tree.sel = (tree.sel + page).min(last),
            Key::Home => tree.sel = 0,
            Key::End => tree.sel = last,
            Key::Right => {
                if !tree.open(tree.sel) && tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.sel = (tree.sel + 1).min(tree.rows.len() - 1);
                }
            }
            Key::Left => {
                if tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.close(tree.sel);
                } else if let Some(parent) = tree.parent_row(tree.sel) {
                    tree.sel = parent;
                }
            }
            Key::Backspace => tree.sel = tree.parent_row(tree.sel).unwrap_or(tree.sel),
            Key::Enter => {
                let sel = tree.sel;
                if tree.rows.get(sel).is_some_and(|n| n.open) {
                    tree.close(sel);
                } else if !tree.open(sel) {
                    return self.enter(); // a file, or an empty folder
                }
            }
            Key::F(1) => self.show_help(),
            Key::Mouse { button, x, y, press: true } => self.tree_mouse(button, x, y),
            // Typing goes back to the search.
            Key::Char(c) => {
                self.tree = None;
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            _ => return self.action_key(key),
        }
        Ok(None)
    }

    fn tree_mouse(&mut self, button: u16, x: usize, y: usize) {
        let body = self.body_rows();
        let Some(tree) = self.tree.as_mut() else { return };
        let last = tree.rows.len().saturating_sub(1);
        match button {
            64 => tree.sel = tree.sel.saturating_sub(3),
            65 => tree.sel = (tree.sel + 3).min(last),
            0 if y >= 1 && y <= body => {
                let row = tree.top + y - 1;
                let Some(node) = tree.rows.get(row) else { return };
                let on_arrow = x >= 2 + 2 * node.depth as usize && x < 4 + 2 * node.depth as usize;
                let double = self.last_click.1 == row && self.last_click.0.elapsed() < Duration::from_millis(400);
                tree.sel = row;
                self.last_click = (Instant::now(), row);
                if on_arrow || double {
                    if node.open {
                        tree.close(row);
                    } else {
                        tree.open(row);
                    }
                }
            }
            _ => {}
        }
    }

    // ---- drawing ------------------------------------------------------------------

    fn draw(&mut self) -> io::Result<()> {
        self.size = self.term.size();
        let (rows, cols) = self.size;
        let mut out = String::from("\x1b[?2026h\x1b[?25l"); // draw the frame at once
        if rows < 4 || cols < 24 {
            out.push_str("\x1b[H\x1b[2JTerminal too small\x1b[?2026l");
            return self.term.tty.write_all(out.as_bytes());
        }
        let body = self.body_rows();
        let mut lines: Vec<String> = Vec::with_capacity(rows);
        let caret = if self.tree.is_some() {
            let path = self.tree.as_ref().and_then(|t| t.selected()).map(|id| clean(&self.path(id))).unwrap_or_default();
            lines.push(format!("\x1b[0m{}Folder sizes{}  {}", self.paint.accent, "\x1b[0m", fit_end(&path, cols.saturating_sub(15))));
            None
        } else {
            let (line, caret) = self.prompt_line(cols);
            lines.push(line);
            Some(caret)
        };
        match &self.panel {
            Some((title, items)) => self.panel_lines(title, items, body, cols, &mut lines),
            None if self.tree.is_some() => self.tree_lines(body, cols, &mut lines),
            None => self.result_lines(body, cols, &mut lines),
        }
        while lines.len() < rows - 1 {
            lines.push(String::new());
        }
        lines.push(self.status_line(cols));
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("\x1b[{};1H{line}\x1b[0m\x1b[K", i + 1));
        }
        if self.panel.is_none() {
            self.completion_box(cols, &mut out);
        }
        match caret {
            Some(col) if self.panel.is_none() => out.push_str(&format!("\x1b[1;{}H\x1b[?25h", col + 1)),
            _ => {}
        }
        out.push_str("\x1b[?2026l");
        self.term.tty.write_all(out.as_bytes())
    }

    /// The search line, and the column of the caret on it.
    fn prompt_line(&self, cols: usize) -> (String, usize) {
        let room = cols.saturating_sub(3).max(1);
        // Keep the caret in view: start far enough right.
        let mut start = 0;
        while width(&self.query[start..self.cursor]) >= room {
            start += self.query[start..].chars().next().map_or(1, char::len_utf8);
        }
        let shown: String = self.query[start..].chars().map(|c| if c.is_control() { '?' } else { c }).collect();
        let line = format!("\x1b[0m{}›\x1b[0m {}", self.paint.accent, fit(&shown, room));
        (line, 2 + width(&self.query[start..self.cursor]))
    }

    fn result_lines(&mut self, body: usize, cols: usize, lines: &mut Vec<String>) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + body {
            self.top = self.sel + 1 - body;
        }
        if self.hits.is_empty() {
            let why = if self.index.is_empty() { "The index is empty: run  ff index" } else { "No results" };
            lines.push(format!("  {}{why}", self.paint.dim));
            return;
        }
        let (show_size, show_date) = (cols >= 50, cols >= 76);
        let right = if show_date { 28 } else if show_size { 11 } else { 0 };
        for row in self.top..(self.top + body).min(self.hits.len()) {
            let id = self.hits[row];
            let selected = row == self.sel;
            let base = if selected { "\x1b[0;7m" } else { "\x1b[0m" };
            let marked = self.marks.contains(&id);
            let mut line = String::from(base);
            line.push_str(if marked { "● " } else { "  " });
            let is_dir = self.index.is_dir(id);
            let colour = if is_dir { self.paint.folder } else if self.index.is_symlink(id) { self.paint.link } else { "" };
            let mut name = clean(self.index.name(id));
            if is_dir && !name.ends_with('/') {
                name.push('/');
            }
            let room = cols.saturating_sub(2 + right);
            let name = fit(&name, room);
            let used = width(&name);
            line.push_str(&format!("{colour}{name}{base}"));
            let folder = self.index.parent(id).map(|p| clean(&self.path(p))).unwrap_or_default();
            if room > used + 4 && !folder.is_empty() {
                let folder = fit_end(&folder, room - used - 2);
                line.push_str(&format!("  {}{folder}{base}", self.paint.dim));
            }
            let used = width(&line_text(&line));
            line.push_str(&" ".repeat(cols.saturating_sub(right).saturating_sub(used)));
            if show_size {
                let size = if is_dir { String::new() } else { human_size(self.index.size(id)) };
                line.push_str(&format!(" {size:>9} "));
            }
            if show_date {
                let (y, mo, d, h, mi) = ff_core::local_civil(self.index.mtime(id));
                line.push_str(&format!("{} {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}{base} ", self.paint.dim));
            }
            lines.push(line);
        }
    }

    fn tree_lines(&mut self, body: usize, cols: usize, lines: &mut Vec<String>) {
        let Some(tree) = self.tree.as_mut() else { return };
        tree.sel = tree.sel.min(tree.rows.len().saturating_sub(1));
        if tree.sel < tree.top {
            tree.top = tree.sel;
        } else if tree.sel >= tree.top + body {
            tree.top = tree.sel + 1 - body;
        }
        let tree = &*tree;
        let (full, medium) = (cols >= 80, cols >= 50);
        let right = if full { 41 } else if medium { 18 } else { 10 };
        for row in tree.top..(tree.top + body).min(tree.rows.len()) {
            let node = &tree.rows[row];
            let id = node.id;
            let base = if row == tree.sel { "\x1b[0;7m" } else { "\x1b[0m" };
            let arrow = if tree.kids(id).is_empty() { "  " } else if node.open { "▾ " } else { "▸ " };
            let is_dir = self.index.is_dir(id);
            let colour = if is_dir { self.paint.folder } else { "" };
            let indent = "  ".repeat(node.depth as usize);
            let room = cols.saturating_sub(2 + indent.len() + 2 + right);
            let name = fit(&clean(self.index.name(id)), room);
            let mut line = format!("{base}  {indent}{}{arrow}{base}{colour}{name}{base}", self.paint.dim);
            let used = 2 + indent.len() + 2 + width(&name);
            line.push_str(&" ".repeat(cols.saturating_sub(right).saturating_sub(used)));
            let of = self.index.parent(id).map_or(tree.total(id), |p| tree.total(p));
            let share = if of == 0 { 0.0 } else { tree.total(id) as f64 / of as f64 };
            let size = human_size(tree.total(id));
            if full {
                let files = if is_dir { group_digits(tree.files(id) as u64) } else { String::new() };
                line.push_str(&format!("{}{}{base} {size:>9} {files:>11} {:>6.1}% ", self.paint.accent, bar(share, 10), share * 100.0));
            } else if medium {
                line.push_str(&format!("{size:>9} {:>6.1}% ", share * 100.0));
            } else {
                line.push_str(&format!("{size:>9} "));
            }
            lines.push(line);
        }
    }

    fn panel_lines(&self, title: &str, items: &[(String, String)], body: usize, cols: usize, lines: &mut Vec<String>) {
        lines.push(format!("  {}{title}", self.paint.accent));
        let key_w = items.iter().map(|(k, _)| width(k)).max().unwrap_or(0).min(cols / 2);
        for (k, v) in items.iter().take(body.saturating_sub(1)) {
            let value = fit(&clean(v.as_bytes()), cols.saturating_sub(key_w + 6));
            lines.push(format!("  {}{}\x1b[0m  {value}", self.paint.folder, pad(&fit(k, key_w), key_w)));
        }
    }

    fn status_line(&self, cols: usize) -> String {
        let text = if let Some(tree) = &self.tree {
            match tree.selected() {
                Some(id) => {
                    let files = if self.index.is_dir(id) { format!("  ·  {} files", group_digits(tree.files(id) as u64)) } else { String::new() };
                    format!(" {} ({} bytes){files}  ·  Esc back", human_size(tree.total(id)), group_digits(tree.total(id)))
                }
                None => " The index is empty".into(),
            }
        } else if let Some((note, _)) = &self.note {
            format!(" {note}")
        } else if let Some(error) = self.error.as_ref().filter(|_| self.complete.is_none()) {
            format!(" {}{error}\x1b[0;7m", self.paint.error)
        } else {
            let mut parts = vec![format!("{} of {}", group_digits(self.hits.len() as u64), group_digits(self.index.len() as u64))];
            if !self.marks.is_empty() {
                parts.push(format!("{} marked", self.marks.len()));
            }
            parts.push(["all", "files only", "folders only"][self.kind as usize].to_string());
            parts.push(format!("case: {}", ["smart", "sensitive", "insensitive"][self.case as usize]));
            if self.regex {
                parts.push("regex".into());
            }
            if self.word {
                parts.push("whole words".into());
            }
            if self.sort != 0 {
                parts.push(format!("by {}", SORTS[self.sort].2));
            }
            parts.push(format!("{} ms", self.took.as_millis()));
            format!(" {}", parts.join("  ·  "))
        };
        let hint = "F1 keys ";
        let room = cols.saturating_sub(hint.len());
        let text = fit(&line_text(&text), room);
        format!("\x1b[0;7m{}{hint}", pad(&text, room))
    }

    /// The completion list, drawn over the results under the word.
    fn completion_box(&self, cols: usize, out: &mut String) {
        let Some(c) = &self.complete else { return };
        let left = width(&self.query[..c.start.min(self.cursor)]);
        let text_w = c.items.iter().map(|(t, _)| width(t)).max().unwrap_or(0);
        let mean_w = c.items.iter().map(|(_, m)| width(m)).max().unwrap_or(0);
        let w = (text_w + mean_w + 5).min(cols);
        let x = (2 + left).min(cols.saturating_sub(w));
        for (i, (text, means)) in c.items.iter().enumerate().take(self.body_rows()) {
            // The chosen entry in the accent colour, so it does not look
            // like the selected result under the list.
            let style = if i == c.chosen { "\x1b[0;1;30;43m" } else { "\x1b[0;100;97m" };
            let inner = format!(" {}  {} ", pad(text, text_w), pad(means, mean_w));
            out.push_str(&format!("\x1b[{};{}H{style}{}\x1b[0m", i + 2, x + 1, fit(&inner, w)));
        }
    }
}

/// The visible text of a line: without its escape codes.
fn line_text(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: up to and including the final letter.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The editor on these files; `read_only` adds the flag that makes the
/// common terminal editors refuse to save. (Any other editor cannot save
/// either: the user has no permission to.)
#[cfg(unix)]
fn editor_command(editor: &str, paths: &[PathBuf], read_only: bool) -> Command {
    let program = editor.split_whitespace().next().unwrap_or("");
    let flag = match program.rsplit('/').next().unwrap_or(program) {
        "vi" | "vim" | "nvim" | "view" | "gvim" | "mvim" if read_only => " -R",
        "nano" if read_only => " -v",
        "micro" if read_only => " -readonly true",
        _ => "",
    };
    let mut command = Command::new("sh");
    // The editor setting may hold arguments ("code -w"): the shell splits it.
    command.arg("-c").arg(format!("{editor}{flag} \"$@\"")).arg("sh").args(paths);
    command
}

/// On Windows the editor setting is split on spaces (`code -w`), with a
/// quoted program path kept whole (`"C:\Program Files\x\x.exe" -n`).
#[cfg(windows)]
fn editor_command(editor: &str, paths: &[PathBuf], read_only: bool) -> Command {
    let editor = editor.trim();
    let (program, rest) = match editor.strip_prefix('"').and_then(|e| e.split_once('"')) {
        Some((program, rest)) => (program, rest),
        None => editor.split_once(' ').unwrap_or((editor, "")),
    };
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program).trim_end_matches(".exe").to_ascii_lowercase();
    let mut command = Command::new(program);
    command.args(rest.split_whitespace());
    if read_only {
        match name.as_str() {
            "vi" | "vim" | "nvim" | "view" | "gvim" => {
                command.arg("-R");
            }
            "nano" => {
                command.arg("-v");
            }
            _ => {}
        }
    }
    command.args(paths);
    command
}

/// The editor when neither `$VISUAL` nor `$EDITOR` is set.
#[cfg(unix)]
const DEFAULT_EDITOR: &str = "vi";
#[cfg(windows)]
const DEFAULT_EDITOR: &str = "notepad";

/// Runs a program that has the terminal, waiting for it. Ctrl+C at a
/// password prompt, or in the editor, is meant for it: like system(3),
/// we ignore it while it runs, and it gets the usual handling.
#[cfg(unix)]
fn run_ignoring_ctrl_c(mut command: Command) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    // SAFETY: signal() is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            sys::signal(sys::SIGINT, sys::SIG_DFL);
            sys::signal(sys::SIGQUIT, sys::SIG_DFL);
            Ok(())
        });
    }
    // SAFETY: setting and restoring dispositions; no handler code runs.
    let saved = unsafe { (sys::signal(sys::SIGINT, sys::SIG_IGN), sys::signal(sys::SIGQUIT, sys::SIG_IGN)) };
    let status = command.status();
    unsafe {
        sys::signal(sys::SIGINT, saved.0);
        sys::signal(sys::SIGQUIT, saved.1);
    }
    status
}

/// On Windows, Ctrl+C reaches every program on the console; a handler
/// that swallows it keeps us alive (handlers are not inherited, so the
/// editor still gets its own).
#[cfg(windows)]
fn run_ignoring_ctrl_c(mut command: Command) -> io::Result<std::process::ExitStatus> {
    // SAFETY: adding and removing a handler that only returns a value.
    unsafe { sys::SetConsoleCtrlHandler(Some(sys::ignore_ctrl_c), 1) };
    let status = command.status();
    unsafe { sys::SetConsoleCtrlHandler(Some(sys::ignore_ctrl_c), 0) };
    status
}

/// True if we may use the file in a way (`mode`: 4 read, 2 write), as
/// `access(2)` says, which knows about groups and read-only filesystems.
#[cfg(unix)]
fn may(path: &Path, mode: i32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn access(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> std::ffi::c_int;
    }
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: a NUL-terminated string that outlives the call.
    unsafe { access(path.as_ptr(), mode) == 0 }
}

/// Only asked on the way to sudoedit, which Windows does not have.
#[cfg(windows)]
fn may(_: &Path, _: i32) -> bool {
    true
}

/// True when a graphical desktop is there to open files on.
#[cfg(not(windows))]
fn desktop() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()))
}

/// A Windows session always has one (over SSH there is no window station
/// to show it on, but opening then fails with a message).
#[cfg(windows)]
fn desktop() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(mut b: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        while let Some((key, n)) = parse_key(b) {
            out.push(key);
            b = &b[n..];
        }
        out
    }

    #[test]
    fn keys_from_terminal_bytes() {
        assert_eq!(keys(b"a\xc3\xa9\r\t\x7f"), [Key::Char('a'), Key::Char('é'), Key::Enter, Key::Tab, Key::Backspace]);
        assert_eq!(keys(b"\x1b[A\x1b[B\x1b[1;3B\x1bOP\x1b[17~\x1b[5~"), [Key::Up, Key::Down, Key::AltDown, Key::F(1), Key::F(6), Key::PageUp]);
        assert_eq!(keys(b"\x03\x0f\x00\x1b1\x1b\r"), [Key::Ctrl('c'), Key::Ctrl('o'), Key::CtrlSpace, Key::Alt('1'), Key::AltEnter]);
        assert_eq!(keys(b"\x1b[1;5C\x1b[3~\x1b[Z"), [Key::CtrlRight, Key::Delete, Key::BackTab]);
        assert_eq!(keys(b"\x1b[<0;10;3M\x1b[<65;1;1M"), [Key::Mouse { button: 0, x: 9, y: 2, press: true }, Key::Mouse { button: 65, x: 0, y: 0, press: true }]);
        // Halves of a sequence wait for the rest.
        assert_eq!(parse_key(b"\x1b"), None);
        assert_eq!(parse_key(b"\x1b[1;"), None);
        assert_eq!(parse_key(b"\xc3"), None);
    }

    #[test]
    fn text_fits_its_cells() {
        assert_eq!(fit("report.pdf", 20), "report.pdf");
        assert_eq!(fit("report.pdf", 6), "repor…");
        assert_eq!(fit_end("/usr/share/doc", 8), "…are/doc");
        assert_eq!(width("日本語"), 6);
        assert_eq!(fit("日本語", 5), "日本…");
        // Escape codes and line breaks in a file name are not passed on.
        assert_eq!(clean(b"evil\x1b[2Jname\n"), "evil?[2Jname?");
        assert_eq!(line_text("\x1b[0;7mab\x1b[1;34mc\x1b[0m"), "abc");
    }

    #[test]
    fn helpers() {
        assert_eq!(base64(b"/tmp/a b"), "L3RtcC9hIGI=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(bar(0.5, 4), "██  ");
        assert_eq!(bar(1.0, 2), "██");
    }
}
