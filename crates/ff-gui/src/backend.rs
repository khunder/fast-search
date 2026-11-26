//! What the application needs from a display server, independent of which
//! one it is. Two implementations exist: X11 (`x11_backend`) and Wayland
//! (`wayland`); `connect` picks the one that matches the session.

use std::io;
use std::time::Duration;

use crate::canvas::Canvas;
pub use crate::keys::Key;

/// One of our windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Win {
    /// A search window. Number 0 is always there (hidden while closed to
    /// the notification area); more are opened by the global shortcut,
    /// each with its own search, as in Everything.
    Main(u8),
    /// The modal dialog (confirmations, help, options); at most one.
    Dialog,
}

/// The first search window, which stays (hidden) while the program runs.
pub const FIRST: Win = Win::Main(0);

#[derive(Clone, Copy, Default, Debug)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Something that happened, in terms the application understands.
/// What the mouse pointer looks like.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cursor {
    Arrow,
    /// The I-beam, over text that can be edited.
    Text,
    /// Over a window edge that resizes: 0 top-left, clockwise to 7 left.
    Resize(u32),
}

#[derive(Debug)]
#[allow(dead_code)] // every event names its window, needed or not
pub enum Event {
    /// The window's content was lost or never shown: draw it again.
    Redraw(Win),
    /// A search window now has this size in pixels.
    Resize(Win, usize, usize),
    /// The user asked to close the window.
    Close(Win),
    Key { win: Win, key: Key, mods: Mods },
    /// A mouse button went down: 1 left, 2 middle, 3 right. `time` is in
    /// milliseconds, for telling double clicks.
    Press { win: Win, button: u8, x: i32, y: i32, time: u32, mods: Mods },
    Release { win: Win, button: u8 },
    Motion { win: Win, x: i32, y: i32, mods: Mods },
    /// The wheel turned; positive is down.
    Scroll { win: Win, lines: i32 },
    /// Text arrived from the clipboard after `request_paste`.
    Paste(String),
    /// During a drag: whether the place under the pointer would take the
    /// drop, and whether it would move the files (else copy them).
    DragStatus { accepted: bool, moves: bool },
    /// The drag was dropped on something that took it.
    DragDropped { moves: bool },
    /// The program that received the drop has finished with it.
    DragFinished,
    /// The drag is over (after a drop, or released over nothing).
    DragEnded,
    /// The first window was shown again, from the notification area.
    Shown,
    /// The global shortcut was pressed, or the program started again:
    /// show the first window if it is hidden, else open another one.
    Summoned,
    /// Exit, chosen outside the window (the notification-area menu).
    Quit,
    /// The desktop's scale for our window became known or changed.
    Scale(f32),
}

pub trait Backend {
    /// "X11", "Wayland" or "Win32".
    fn name(&self) -> &'static str;

    /// The desktop scale: 1.0 is 96 dpi.
    fn scale(&mut self) -> f32;

    /// Waits for the next event: forever (`None`), not at all (zero), or
    /// up to a timeout; `Ok(None)` means none arrived in that time.
    fn next_event(&mut self, wait: Option<Duration>) -> io::Result<Option<Event>>;

    /// Shows a canvas in a window.
    fn present(&mut self, win: Win, canvas: &Canvas) -> io::Result<()>;

    /// Whether we draw the title bar and frame ourselves (true) or ask
    /// the desktop to (false).
    /// Returns what is in effect: a desktop that draws no title bars
    /// leaves it to us whatever was asked.
    fn set_own_title_bar(&mut self, own: bool) -> bool;
    /// Opens another search window like the first, in front; returns its
    /// number. It arrives with a `Resize`.
    fn open_window(&mut self) -> io::Result<u8> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "one window only"))
    }

    /// Closes search window `n` (not the first, which only hides).
    fn close_window(&mut self, _n: u8) {}

    /// The search window the calls without a window of their own act on
    /// (moving, resizing, cursor, minimize, clipboard owner, drag source,
    /// dialog owner).
    fn select(&mut self, _n: u8) {}

    /// The selected window's system handle (Windows: its HWND), for dialogs
    /// the system shows on its behalf; 0 where there is none.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn native_window(&self) -> isize {
        0
    }

    /// Shows the first window again, in front, after `hide_to_tray`.
    fn show(&mut self) {}

    /// Makes `keys` (as in "Ctrl+Alt+F") show the main window from anywhere
    /// while the program runs, or drops it (None). Platforms whose desktop
    /// keeps the shortcut itself (GNOME) do nothing here.
    fn set_hotkey(&mut self, _keys: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    /// Hides the first window to the notification area instead of closing
    /// it (Windows), or just out of sight (elsewhere: there is no common
    /// tray); false if it cannot (then close).
    fn hide_to_tray(&mut self) -> bool {
        false
    }

    /// True if the desktop gives the dialog window no title bar, so the
    /// dialog has to draw one.
    fn dialog_needs_title_bar(&self) -> bool;

    /// Hands the current mouse press to the desktop as a window move, or
    /// a resize from an edge (0 top-left, then clockwise to 7 left).
    fn start_move(&mut self);
    /// The mouse pointer's shape over the main window.
    fn set_cursor(&mut self, cursor: Cursor);
    fn start_resize(&mut self, edge: u32);
    fn minimize(&mut self);
    fn toggle_maximize(&mut self);

    /// Puts text on the clipboard; with `file`, each line is the path of
    /// a file offered as a file too. Returns false if that failed.
    fn copy(&mut self, text: &str, file: bool) -> bool;

    /// Asks for the clipboard's text; it arrives as `Event::Paste`.
    fn request_paste(&mut self);

    /// Called before exiting: keep what we copied available if possible.
    fn keep_clipboard(&mut self);

    /// Opens the dialog window with this size; closes it again.
    fn open_dialog(&mut self, w: i32, h: i32, title: &str) -> io::Result<()>;
    fn close_dialog(&mut self);

    /// Starts dragging files (one path per line) from the current mouse
    /// press, with `image` following the pointer. `copy` asks targets to
    /// copy rather than move; `drag_wants_copy` updates that mid-drag.
    fn start_drag(&mut self, paths: &str, image: &Canvas, copy: bool) -> io::Result<()>;
    fn drag_wants_copy(&mut self, copy: bool);
}

/// What a new main window should look like.
pub struct WindowSpec<'a> {
    pub title: &'a str,
    /// Size at a scale of 1; the backend multiplies by the scale it finds.
    pub width: i32,
    pub height: i32,
    /// Start hidden: in the notification area only (Windows; elsewhere the
    /// window shows as usual).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub hidden: bool,
    /// Fixed scale from the user, overriding the desktop's.
    pub scale: Option<f32>,
}

/// Connects to the session's display server and creates the main window.
/// A Wayland session gets the Wayland backend, anything else X11;
/// `FF_GUI_BACKEND=x11` or `=wayland` forces one. If the preferred one
/// cannot be reached, the other is tried.
#[cfg(not(windows))]
pub fn connect(spec: &WindowSpec) -> io::Result<Box<dyn Backend>> {
    let forced = std::env::var("FF_GUI_BACKEND").ok();
    let wayland_session = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let x11 = |spec: &WindowSpec| crate::x11_backend::X11::connect(spec).map(|b| Box::new(b) as Box<dyn Backend>);
    let wayland = |spec: &WindowSpec| crate::wayland::Wayland::connect(spec).map(|b| Box::new(b) as Box<dyn Backend>);
    match forced.as_deref() {
        Some("x11") => x11(spec),
        Some("wayland") => wayland(spec),
        _ if wayland_session => wayland(spec).or_else(|wayland_error| {
            // Running through XWayland still works when Wayland does not.
            x11(spec).map_err(|_| wayland_error)
        }),
        _ => x11(spec).or_else(|x11_error| wayland(spec).map_err(|_| x11_error)),
    }
}

/// On Windows there is one choice: the Win32 window manager.
#[cfg(windows)]
pub fn connect(spec: &WindowSpec) -> io::Result<Box<dyn Backend>> {
    crate::win32::Win32::connect(spec).map(|b| Box::new(b) as Box<dyn Backend>)
}
