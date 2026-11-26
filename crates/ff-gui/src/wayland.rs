//! The native Wayland backend: the wire protocol spoken directly over the
//! compositor's Unix socket, with no libwayland. It covers what the window
//! needs: shared-memory buffers, xdg-shell windows, pointer and keyboard,
//! the clipboard, and dragging files out. It works with any compositor
//! that has xdg-shell: GNOME, KDE, Hyprland, Sway and the like.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::backend::{Backend, Cursor, Event, FIRST, Key, Mods, Win, WindowSpec};
use crate::canvas::Canvas;
use crate::debug;
use crate::xkb::{self, Keymap};

use A::{Fd, I, S, U};

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

// ---- the socket ---------------------------------------------------------------

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

/// `struct msghdr` on 64-bit Linux.
#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut u8,
    control_len: usize,
    flags: i32,
}

unsafe extern "C" {
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
}

const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_NOSIGNAL: i32 = 0x4000;
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;

/// One argument of a request.
enum A<'a> {
    U(u32),
    I(i32),
    S(&'a str),
    Fd(RawFd),
}

/// The connection: requests out, events in, file descriptors both ways.
struct Wire {
    sock: UnixStream,
    out: Vec<u8>,
    inb: Vec<u8>,
    /// Descriptors received and not yet claimed by an event.
    fds: VecDeque<OwnedFd>,
    next_id: u32,
    /// The first error met while sending; reported by `flush`.
    failed: Option<io::Error>,
}

impl Wire {
    fn new(sock: UnixStream) -> Wire {
        Wire { sock, out: Vec::new(), inb: Vec::new(), fds: VecDeque::new(), next_id: 2, failed: None }
    }

    fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// Queues a request to `object`.
    fn send(&mut self, object: u32, opcode: u16, args: &[A]) {
        let start = self.out.len();
        self.out.extend([0u8; 8]);
        let mut fd = None;
        for arg in args {
            match arg {
                U(v) => self.out.extend(v.to_le_bytes()),
                I(v) => self.out.extend(v.to_le_bytes()),
                S(text) => {
                    self.out.extend((text.len() as u32 + 1).to_le_bytes());
                    self.out.extend(text.as_bytes());
                    self.out.extend(&[0u8; 4][..4 - text.len() % 4]);
                }
                Fd(raw) => fd = Some(*raw),
            }
        }
        let size = (self.out.len() - start) as u32;
        self.out[start..start + 4].copy_from_slice(&object.to_le_bytes());
        self.out[start + 4..start + 8].copy_from_slice(&(size << 16 | opcode as u32).to_le_bytes());
        if let Some(fd) = fd {
            // A descriptor travels beside the bytes of its own request.
            let message = self.out.split_off(start);
            let sent = self.flush().and_then(|()| self.send_with_fd(&message, fd));
            if let Err(e) = sent {
                self.failed.get_or_insert(e);
            }
        }
    }

    fn send_with_fd(&mut self, bytes: &[u8], fd: RawFd) -> io::Result<()> {
        // One control message: length 20 (header 16 + one descriptor).
        let mut control = [0u64; 3];
        control[0] = 20;
        control[1] = SOL_SOCKET as u64 | (SCM_RIGHTS as u64) << 32;
        control[2] = fd as u32 as u64;
        let mut iov = IoVec { base: bytes.as_ptr() as *mut u8, len: bytes.len() };
        let header = MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &mut iov,
            iov_len: 1,
            control: control.as_mut_ptr() as *mut u8,
            control_len: 24,
            flags: 0,
        };
        loop {
            // SAFETY: the header points at buffers that live through the call.
            let n = unsafe { sendmsg(self.sock.as_raw_fd(), &header, MSG_NOSIGNAL) };
            if n >= 0 {
                // The descriptor went with the first byte; the rest is plain.
                return self.sock.write_all(&bytes[n as usize..]);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        if !self.out.is_empty() {
            self.sock.write_all(&self.out)?;
            self.out.clear();
        }
        Ok(())
    }

    /// The next event as (object, opcode, arguments). `wait` is how long to
    /// wait for one: forever (`None`), not at all (zero), or up to a time.
    fn read(&mut self, wait: Option<Duration>) -> io::Result<Option<(u32, u16, Vec<u8>)>> {
        loop {
            if self.inb.len() >= 8 {
                let word = |i: usize| u32::from_le_bytes([self.inb[i], self.inb[i + 1], self.inb[i + 2], self.inb[i + 3]]);
                let (object, info) = (word(0), word(4));
                let size = ((info >> 16) as usize).max(8);
                if self.inb.len() >= size {
                    let body = self.inb[8..size].to_vec();
                    self.inb.drain(..size);
                    return Ok(Some((object, info as u16, body)));
                }
            }
            let mut chunk = [0u8; 8192];
            let mut control = [0u64; 32];
            let mut iov = IoVec { base: chunk.as_mut_ptr(), len: chunk.len() };
            let mut header = MsgHdr {
                name: std::ptr::null_mut(),
                name_len: 0,
                iov: &mut iov,
                iov_len: 1,
                control: control.as_mut_ptr() as *mut u8,
                control_len: std::mem::size_of_val(&control),
                flags: 0,
            };
            let mut flags = MSG_CMSG_CLOEXEC;
            match wait {
                None => {}
                Some(Duration::ZERO) => flags |= MSG_DONTWAIT,
                Some(timeout) => self.sock.set_read_timeout(Some(timeout))?,
            }
            // SAFETY: the header points at buffers that live through the call.
            let n = unsafe { recvmsg(self.sock.as_raw_fd(), &mut header, flags) };
            let error = (n < 0).then(io::Error::last_os_error);
            if wait.is_some_and(|w| !w.is_zero()) {
                self.sock.set_read_timeout(None)?;
            }
            if let Some(e) = error {
                match e.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => return Ok(None),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(e),
                }
            }
            if n == 0 {
                return Err(err("the Wayland compositor closed the connection"));
            }
            self.inb.extend(&chunk[..n as usize]);
            // Descriptors: control messages of 16 header bytes, then 32-bit
            // numbers, each message padded to 8 bytes.
            let bytes: Vec<u8> = control.iter().flat_map(|w| w.to_le_bytes()).collect();
            let mut at = 0;
            while at + 16 <= header.control_len.min(bytes.len()) {
                let field = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                let len = (u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize).max(16);
                if field(at + 8) == SOL_SOCKET && field(at + 12) == SCM_RIGHTS {
                    for i in (at + 16..(at + len).min(bytes.len()).saturating_sub(3)).step_by(4) {
                        // SAFETY: the kernel just gave us this descriptor.
                        self.fds.push_back(unsafe { OwnedFd::from_raw_fd(field(i) as i32) });
                    }
                }
                at += len.div_ceil(8) * 8;
            }
        }
    }
}

/// Reads the arguments of an event; missing bytes read as zero.
struct Args<'a>(&'a [u8]);

impl Args<'_> {
    fn u(&mut self) -> u32 {
        let Some((head, rest)) = self.0.split_at_checked(4) else { return 0 };
        self.0 = rest;
        u32::from_le_bytes([head[0], head[1], head[2], head[3]])
    }

    fn i(&mut self) -> i32 {
        self.u() as i32
    }

    /// A 24.8 fixed-point number.
    fn fixed(&mut self) -> f64 {
        self.i() as f64 / 256.0
    }

    fn bytes(&mut self) -> &[u8] {
        let len = self.u() as usize;
        let padded = len.div_ceil(4) * 4;
        let Some((head, rest)) = self.0.split_at_checked(padded) else { return &[] };
        self.0 = rest;
        &head[..len]
    }

    fn string(&mut self) -> String {
        let bytes = self.bytes();
        String::from_utf8_lossy(bytes.strip_suffix(&[0]).unwrap_or(bytes)).into_owned()
    }
}

/// An unnamed file in memory to share with the compositor.
fn memory_file(size: usize) -> io::Result<File> {
    // SAFETY: the name is a NUL-terminated string; 1 is MFD_CLOEXEC.
    let fd = unsafe { memfd_create(c"ff-gui".as_ptr() as *const u8, 1) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor that nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file)
}

// ---- objects ------------------------------------------------------------------

/// What an object id stands for, to route its events.
#[derive(Clone, Copy, PartialEq)]
enum Obj {
    Registry,
    Sync,
    WmBase,
    Seat,
    Pointer,
    Keyboard,
    Output,
    Surface(Win),
    XdgSurface(Win),
    Toplevel(Win),
    Buffer,
    DataDevice,
    Offer,
    ClipSource,
    DragSource,
    /// A data source we destroyed: a late request to send must still
    /// have its descriptor closed.
    DeadSource,
    Fractional,
}

/// A pixel buffer shared with the compositor.
struct Buffer {
    id: u32,
    file: File,
    w: usize,
    h: usize,
    /// The compositor is still showing it.
    busy: bool,
}

#[derive(Default)]
struct Window {
    surface: u32,
    xdg: u32,
    toplevel: u32,
    viewport: u32,
    decoration: u32,
    fractional: u32,
    /// Size in the compositor's units (pixels divided by the scale).
    lw: i32,
    lh: i32,
    /// Size from the last toplevel configure, waiting for its commit.
    asked: (i32, i32),
    configured: bool,
    maximized: bool,
    buffers: Vec<Buffer>,
}

struct Drag {
    source: u32,
    icon: u32,
    icon_viewport: u32,
    icon_buffer: Option<Buffer>,
    /// Whether the place under the pointer takes the data, and how.
    accepted: bool,
    action: u32,
}

const MIME_TEXT: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];
const MIME_URIS: &str = "text/uri-list";
const MIME_GNOME_FILES: &str = "x-special/gnome-copied-files";

/// What to write for a request of our data in a format.
fn data_for(mime: &str, text: &str, files: bool) -> Vec<u8> {
    let uris = || text.lines().map(|p| ff_core::desktop::file_uri(std::path::Path::new(p))).collect::<Vec<_>>();
    match mime {
        MIME_URIS if files => (uris().join("\r\n") + "\r\n").into_bytes(),
        MIME_GNOME_FILES if files => format!("copy\n{}", uris().join("\n")).into_bytes(),
        _ => text.as_bytes().to_vec(),
    }
}

/// Writes to a pipe without making the window wait for its reader.
fn write_later(fd: OwnedFd, data: Vec<u8>) {
    std::thread::spawn(move || {
        let _ = File::from(fd).write_all(&data);
    });
}

pub struct Wayland {
    w: Wire,
    objs: HashMap<u32, Obj>,
    pending: VecDeque<Event>,
    sync_done: bool,

    // Globals (0 when the compositor does not have one).
    registry: u32,
    compositor: u32,
    shm: u32,
    wm_base: u32,
    seat: u32,
    seat_version: u32,
    data_manager: u32,
    data_version: u32,
    decorations: u32,
    viewporter: u32,
    fractional: u32,
    cursor_shapes: u32,
    /// Scale of each output.
    outputs: HashMap<u32, i32>,

    scale: f32,
    /// The window size to start with at a scale of 1, and the scale the
    /// user fixed for the layout, if any.
    wanted: (i32, i32),
    fixed_scale: Option<f32>,
    /// The first search window.
    main: Window,
    /// The other search windows by number (slot 0 unused; None: closed).
    others: Vec<Option<Window>>,
    /// The search window `select` chose.
    selected: u8,
    /// The main window's title, and whether we draw its title bar: to
    /// make it again after it was hidden.
    title: String,
    own_title: bool,
    /// The main window is taken down (closed but still running).
    hidden: bool,
    dialog: Option<Window>,
    /// The pixel size last announced to the application, by window.
    announced: Vec<(usize, usize)>,

    pointer: u32,
    cursor_device: u32,
    /// The shape wanted over the main window, and the serial of the
    /// pointer's last entry, which a change of shape must quote.
    cursor: Cursor,
    enter_serial: u32,
    cursor_surface: u32,
    pointer_focus: Option<Win>,
    pointer_at: (i32, i32),
    /// Wheel movement collected until the end of the pointer frame.
    wheel_steps: i32,
    wheel_distance: f64,
    wheel_rest: f64,

    keyboard: u32,
    keyboard_focus: Option<Win>,
    keymap: Keymap,
    mods: u32,
    group: u32,
    /// Key repeat: characters per second, and milliseconds before the first.
    repeat_rate: i32,
    repeat_delay: i32,
    repeating: Option<(u32, Win, Instant)>,

    /// Serial of the latest input event, and of the latest button press.
    serial: u32,
    press_serial: u32,
    press_win: Win,

    data_device: u32,
    clip_source: u32,
    copied: String,
    copied_files: bool,
    /// Formats of each data offer, and the offer that is the clipboard.
    offers: HashMap<u32, Vec<String>>,
    selection: u32,
    drop_offer: u32,
    drag: Option<Drag>,
    drag_paths: String,
}

impl Wayland {
    pub fn connect(spec: &WindowSpec) -> io::Result<Wayland> {
        let name = std::env::var_os("WAYLAND_DISPLAY").filter(|v| !v.is_empty()).unwrap_or("wayland-0".into());
        let mut path = std::path::PathBuf::from(name);
        if path.is_relative() {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| err("XDG_RUNTIME_DIR is not set"))?;
            path = std::path::Path::new(&runtime).join(path);
        }
        let sock = UnixStream::connect(&path).map_err(|e| err(format!("cannot reach Wayland at {}: {e}", path.display())))?;
        let mut wl = Wayland {
            w: Wire::new(sock),
            objs: HashMap::new(),
            pending: VecDeque::new(),
            sync_done: false,
            registry: 2,
            compositor: 0,
            shm: 0,
            wm_base: 0,
            seat: 0,
            seat_version: 0,
            data_manager: 0,
            data_version: 0,
            decorations: 0,
            viewporter: 0,
            fractional: 0,
            cursor_shapes: 0,
            outputs: HashMap::new(),
            scale: 1.0,
            wanted: (spec.width, spec.height),
            fixed_scale: spec.scale,
            main: Window::default(),
            others: vec![None],
            selected: 0,
            title: spec.title.to_string(),
            own_title: false,
            hidden: false,
            dialog: None,
            announced: vec![(0, 0)],
            pointer: 0,
            cursor_device: 0,
            cursor: Cursor::Arrow,
            enter_serial: 0,
            cursor_surface: 0,
            pointer_focus: None,
            pointer_at: (0, 0),
            wheel_steps: 0,
            wheel_distance: 0.0,
            wheel_rest: 0.0,
            keyboard: 0,
            keyboard_focus: None,
            keymap: Keymap::default(),
            mods: 0,
            group: 0,
            repeat_rate: 25,
            repeat_delay: 500,
            repeating: None,
            serial: 0,
            press_serial: 0,
            press_win: FIRST,
            data_device: 0,
            clip_source: 0,
            copied: String::new(),
            copied_files: false,
            offers: HashMap::new(),
            selection: 0,
            drop_offer: 0,
            drag: None,
            drag_paths: String::new(),
        };
        wl.w.next_id = 3;
        wl.w.send(1, 1, &[U(wl.registry)]); // wl_display.get_registry
        wl.objs.insert(wl.registry, Obj::Registry);
        wl.roundtrip()?; // the globals
        wl.roundtrip()?; // what they say about themselves: seat, outputs
        if wl.compositor == 0 || wl.shm == 0 || wl.wm_base == 0 {
            return Err(err("the Wayland compositor lacks wl_compositor, wl_shm or xdg_wm_base"));
        }
        if wl.data_manager != 0 && wl.seat != 0 {
            wl.data_device = wl.w.new_id();
            wl.w.send(wl.data_manager, 1, &[U(wl.data_device), U(wl.seat)]);
            wl.objs.insert(wl.data_device, Obj::DataDevice);
        }
        // Until the compositor names a scale for the window, the largest
        // output scale is the best guess.
        wl.scale = wl.outputs.values().copied().max().unwrap_or(1).max(1) as f32;

        wl.main = wl.create_window(FIRST, spec.title);
        wl.w.send(wl.main.toplevel, 8, &[I(360), I(200)]); // set_min_size
        wl.w.send(wl.main.surface, 6, &[]); // commit: ask for the first configure
        while !wl.main.configured {
            wl.step(None)?;
        }
        debug::log(format!(
            "wayland: scale {} decorations {} fractional {} cursor shapes {} data v{}",
            wl.scale,
            wl.decorations != 0,
            wl.fractional != 0,
            wl.cursor_shapes != 0,
            wl.data_version
        ));
        Ok(wl)
    }

    /// Reads and handles one event; false if none came in time.
    fn step(&mut self, wait: Option<Duration>) -> io::Result<bool> {
        self.w.flush()?;
        let Some((object, opcode, body)) = self.w.read(wait)? else { return Ok(false) };
        self.dispatch(object, opcode, &body)?;
        Ok(true)
    }

    /// Waits until the compositor has handled everything sent so far.
    fn roundtrip(&mut self) -> io::Result<()> {
        let callback = self.w.new_id();
        self.w.send(1, 0, &[U(callback)]); // wl_display.sync
        self.objs.insert(callback, Obj::Sync);
        self.sync_done = false;
        while !self.sync_done {
            self.step(None)?;
        }
        Ok(())
    }

    fn bind(&mut self, name: u32, interface: &str, version: u32) -> u32 {
        let id = self.w.new_id();
        self.w.send(self.registry, 0, &[U(name), S(interface), U(version), U(id)]);
        id
    }

    fn global(&mut self, name: u32, interface: &str, version: u32) {
        match interface {
            "wl_compositor" => self.compositor = self.bind(name, interface, version.min(4)),
            "wl_shm" => self.shm = self.bind(name, interface, 1),
            "xdg_wm_base" => {
                self.wm_base = self.bind(name, interface, 1);
                self.objs.insert(self.wm_base, Obj::WmBase);
            }
            "wl_seat" if self.seat == 0 => {
                self.seat_version = version.min(5);
                self.seat = self.bind(name, interface, self.seat_version);
                self.objs.insert(self.seat, Obj::Seat);
            }
            "wl_data_device_manager" => {
                self.data_version = version.min(3);
                self.data_manager = self.bind(name, interface, self.data_version);
            }
            "wl_output" if version >= 2 => {
                let output = self.bind(name, interface, 2);
                self.objs.insert(output, Obj::Output);
                self.outputs.insert(output, 1);
            }
            "zxdg_decoration_manager_v1" => self.decorations = self.bind(name, interface, 1),
            "wp_viewporter" => self.viewporter = self.bind(name, interface, 1),
            "wp_fractional_scale_manager_v1" => self.fractional = self.bind(name, interface, 1),
            "wp_cursor_shape_manager_v1" => self.cursor_shapes = self.bind(name, interface, 1),
            _ => {}
        }
    }

    // ---- windows ----------------------------------------------------------

    fn create_window(&mut self, win: Win, title: &str) -> Window {
        let mut window = Window { surface: self.w.new_id(), xdg: self.w.new_id(), toplevel: self.w.new_id(), ..Window::default() };
        self.w.send(self.compositor, 0, &[U(window.surface)]); // create_surface
        self.w.send(self.wm_base, 2, &[U(window.xdg), U(window.surface)]); // get_xdg_surface
        self.w.send(window.xdg, 1, &[U(window.toplevel)]); // get_toplevel
        self.objs.insert(window.surface, Obj::Surface(win));
        self.objs.insert(window.xdg, Obj::XdgSurface(win));
        self.objs.insert(window.toplevel, Obj::Toplevel(win));
        self.w.send(window.toplevel, 2, &[S(title)]); // set_title
        self.w.send(window.toplevel, 3, &[S("ff-gui")]); // set_app_id: matches ff-gui.desktop
        if self.viewporter != 0 {
            window.viewport = self.w.new_id();
            self.w.send(self.viewporter, 1, &[U(window.viewport), U(window.surface)]);
        }
        if self.decorations != 0 {
            window.decoration = self.w.new_id();
            self.w.send(self.decorations, 1, &[U(window.decoration), U(window.toplevel)]);
            if win == Win::Dialog {
                self.w.send(window.decoration, 1, &[U(2)]); // set_mode: server side
            }
        }
        if self.fractional != 0 && win != Win::Dialog {
            window.fractional = self.w.new_id();
            self.w.send(self.fractional, 1, &[U(window.fractional), U(window.surface)]);
            self.objs.insert(window.fractional, Obj::Fractional);
        }
        window
    }

    fn destroy_window(&mut self, window: Window) {
        for id in [window.surface, window.xdg, window.toplevel, window.fractional] {
            self.objs.remove(&id);
        }
        // Each role object goes before the one it was made from.
        for id in [window.fractional, window.decoration, window.viewport, window.toplevel, window.xdg, window.surface] {
            if id != 0 {
                self.w.send(id, 0, &[]); // destroy
            }
        }
        for buffer in window.buffers {
            self.objs.remove(&buffer.id);
            self.w.send(buffer.id, 0, &[]);
        }
    }

    fn win_of_surface(&self, surface: u32) -> Option<Win> {
        if surface == 0 {
            None
        } else if surface == self.main.surface {
            Some(FIRST)
        } else if self.dialog.as_ref().is_some_and(|d| d.surface == surface) {
            Some(Win::Dialog)
        } else {
            self.others.iter().position(|w| w.as_ref().is_some_and(|w| w.surface == surface)).map(|n| Win::Main(n as u8))
        }
    }

    fn window(&self, win: Win) -> Option<&Window> {
        match win {
            Win::Main(0) => Some(&self.main),
            Win::Main(n) => self.others.get(n as usize).and_then(Option::as_ref),
            Win::Dialog => self.dialog.as_ref(),
        }
    }

    fn window_mut(&mut self, win: Win) -> Option<&mut Window> {
        match win {
            Win::Main(0) => Some(&mut self.main),
            Win::Main(n) => self.others.get_mut(n as usize).and_then(Option::as_mut),
            Win::Dialog => self.dialog.as_mut(),
        }
    }

    /// The search windows open, by number.
    fn mains(&self) -> Vec<Win> {
        std::iter::once(FIRST).chain((1..self.others.len()).filter(|&n| self.others[n].is_some()).map(|n| Win::Main(n as u8))).collect()
    }

    /// The scale as a whole number, for compositors that take no other.
    fn whole_scale(&self) -> usize {
        (self.scale.ceil() as usize).max(1)
    }

    /// Tells the application a search window's size in pixels if it
    /// changed.
    fn announce_size(&mut self, win: Win) {
        let Win::Main(n) = win else { return };
        let Some((lw, lh)) = self.window(win).map(|w| (w.lw, w.lh)) else { return };
        if lw <= 0 || lh <= 0 {
            return;
        }
        let px = |v: i32| ((v as f32 * self.scale).round() as usize).max(1);
        let size = (px(lw), px(lh));
        if self.announced.len() <= n as usize {
            self.announced.resize(n as usize + 1, (0, 0));
        }
        if size != self.announced[n as usize] {
            self.announced[n as usize] = size;
            self.pending.push_back(Event::Resize(win, size.0, size.1));
        }
    }

    fn new_buffer(&mut self, w: usize, h: usize, format: u32) -> io::Result<Buffer> {
        let size = w * h * 4;
        let file = memory_file(size)?;
        let (pool, id) = (self.w.new_id(), self.w.new_id());
        self.w.send(self.shm, 0, &[U(pool), Fd(file.as_raw_fd()), I(size as i32)]); // create_pool
        self.w.send(pool, 0, &[U(id), I(0), I(w as i32), I(h as i32), I(w as i32 * 4), U(format)]); // create_buffer
        self.w.send(pool, 1, &[]); // destroy: the buffer keeps the memory
        self.objs.insert(id, Obj::Buffer);
        Ok(Buffer { id, file, w, h, busy: false })
    }

    /// Fills a buffer of `bw` x `bh` pixels from a canvas no larger than it;
    /// the extra right and bottom edges repeat the last pixel.
    fn fill_buffer(buffer: &Buffer, cv: &Canvas) -> io::Result<()> {
        if buffer.w == cv.w && buffer.h == cv.h {
            return buffer.file.write_all_at(&cv.px[..cv.w * cv.h * 4], 0);
        }
        let mut px = Vec::with_capacity(buffer.w * buffer.h * 4);
        for y in 0..buffer.h {
            let row = &cv.px[y.min(cv.h - 1) * cv.w * 4..][..cv.w * 4];
            px.extend_from_slice(row);
            for _ in cv.w..buffer.w {
                px.extend_from_slice(&row[row.len() - 4..]);
            }
        }
        buffer.file.write_all_at(&px, 0)
    }

    fn present_window(&mut self, window: &mut Window, cv: &Canvas) -> io::Result<()> {
        if !window.configured || cv.w == 0 || cv.h == 0 || window.lw <= 0 || window.lh <= 0 {
            return Ok(());
        }
        // With a viewport the buffer can have any size and is shown at the
        // window's size; without, its size must be a multiple of the scale.
        let whole = self.whole_scale();
        let (bw, bh) = match window.viewport {
            0 => (cv.w.div_ceil(whole) * whole, cv.h.div_ceil(whole) * whole),
            _ => {
                // The sharpest picture is a buffer of exactly the window's
                // size times the scale; a canvas a pixel short of that
                // (a dialog sized in pixels) is padded rather than stretched.
                let exact = |logical: i32, have: usize| {
                    let want = (logical as f32 * self.scale).round() as usize;
                    if (have..=have + 2).contains(&want) { want } else { have }
                };
                (exact(window.lw, cv.w), exact(window.lh, cv.h))
            }
        };
        let mut i = 0;
        while i < window.buffers.len() {
            let b = &window.buffers[i];
            if !b.busy && (b.w, b.h) != (bw, bh) {
                let old = window.buffers.swap_remove(i);
                self.objs.remove(&old.id);
                self.w.send(old.id, 0, &[]);
            } else {
                i += 1;
            }
        }
        let free = window.buffers.iter().position(|b| !b.busy && (b.w, b.h) == (bw, bh));
        let index = match free {
            Some(index) => index,
            None => {
                let buffer = self.new_buffer(bw, bh, 1)?; // XRGB8888
                window.buffers.push(buffer);
                window.buffers.len() - 1
            }
        };
        let buffer = &mut window.buffers[index];
        Self::fill_buffer(buffer, cv)?;
        buffer.busy = true;
        self.w.send(window.surface, 1, &[U(buffer.id), I(0), I(0)]); // attach
        match window.viewport {
            0 => self.w.send(window.surface, 8, &[I(whole as i32)]), // set_buffer_scale
            viewport => self.w.send(viewport, 2, &[I(window.lw), I(window.lh)]), // set_destination
        }
        self.w.send(window.surface, 2, &[I(0), I(0), I(i32::MAX), I(i32::MAX)]); // damage
        self.w.send(window.surface, 6, &[]); // commit
        Ok(())
    }

    // ---- events -----------------------------------------------------------

    fn mods(&self) -> Mods {
        Mods { shift: self.mods & xkb::SHIFT != 0, ctrl: self.mods & xkb::CTRL != 0, alt: self.mods & xkb::ALT != 0 }
    }

    fn dispatch(&mut self, object: u32, opcode: u16, body: &[u8]) -> io::Result<()> {
        let mut a = Args(body);
        if object == 1 {
            match opcode {
                0 => {
                    let (on, code) = (a.u(), a.u());
                    return Err(err(format!("Wayland protocol error on object {on}, code {code}: {}", a.string())));
                }
                _ => drop(self.objs.remove(&a.u())), // delete_id
            }
            return Ok(());
        }
        let Some(&kind) = self.objs.get(&object) else { return Ok(()) };
        match (kind, opcode) {
            (Obj::Registry, 0) => {
                let (name, interface, version) = (a.u(), a.string(), a.u());
                self.global(name, &interface, version);
            }
            (Obj::Sync, 0) => self.sync_done = true,
            (Obj::WmBase, 0) => self.w.send(object, 3, &[U(a.u())]), // ping -> pong
            (Obj::Seat, 0) => {
                let capabilities = a.u();
                if capabilities & 1 != 0 && self.pointer == 0 {
                    self.pointer = self.w.new_id();
                    self.w.send(self.seat, 0, &[U(self.pointer)]);
                    self.objs.insert(self.pointer, Obj::Pointer);
                    if self.cursor_shapes != 0 {
                        self.cursor_device = self.w.new_id();
                        self.w.send(self.cursor_shapes, 1, &[U(self.cursor_device), U(self.pointer)]);
                    }
                }
                if capabilities & 2 != 0 && self.keyboard == 0 {
                    self.keyboard = self.w.new_id();
                    self.w.send(self.seat, 1, &[U(self.keyboard)]);
                    self.objs.insert(self.keyboard, Obj::Keyboard);
                }
            }
            (Obj::Output, 3) => drop(self.outputs.insert(object, a.i())),
            (Obj::Surface(win), 0) => debug::log(format!("wayland: {win:?} window is shown on output {}", a.u())),
            (Obj::Fractional, 0) => {
                let scale = a.u() as f32 / 120.0;
                if scale > 0.0 && (scale - self.scale).abs() > 0.001 {
                    self.scale = scale;
                    self.pending.push_back(Event::Scale(scale));
                    for win in self.mains() {
                        self.announce_size(win);
                        self.pending.push_back(Event::Redraw(win));
                    }
                }
            }
            (Obj::Toplevel(win), 0) => {
                let (w, h) = (a.i(), a.i());
                let maximized = a.bytes().chunks_exact(4).any(|s| s == [1, 0, 0, 0]);
                if let Some(window) = self.window_mut(win).filter(|_| win != Win::Dialog) {
                    window.asked = (w, h);
                    window.maximized = maximized;
                }
            }
            (Obj::Toplevel(win), 1) => self.pending.push_back(Event::Close(win)),
            (Obj::XdgSurface(win), 0) => {
                self.w.send(object, 4, &[U(a.u())]); // ack_configure
                match win {
                    Win::Main(_) => {
                        // Our choice: the wanted size, in the compositor's units.
                        // A layout zoomed by the user needs that much more room.
                        let zoom = self.fixed_scale.map_or(1.0, |fixed| fixed / self.scale);
                        let wanted = ((self.wanted.0 as f32 * zoom).round().max(1.0) as i32, (self.wanted.1 as f32 * zoom).round().max(1.0) as i32);
                        if let Some(window) = self.window_mut(win) {
                            let (w, h) = window.asked;
                            if w > 0 && h > 0 {
                                (window.lw, window.lh) = (w, h);
                            } else if window.lw == 0 {
                                (window.lw, window.lh) = wanted;
                            }
                            window.configured = true;
                        }
                        self.announce_size(win);
                    }
                    Win::Dialog => {
                        if let Some(dialog) = self.dialog.as_mut() {
                            dialog.configured = true;
                        }
                    }
                }
                self.pending.push_back(Event::Redraw(win));
            }
            (Obj::Buffer, 0) => {
                let windows = std::iter::once(&mut self.main).chain(self.others.iter_mut().flatten()).chain(self.dialog.as_mut());
                if let Some(buffer) = windows.flat_map(|w| w.buffers.iter_mut()).find(|b| b.id == object) {
                    buffer.busy = false;
                }
            }
            (Obj::Pointer, _) => self.pointer_event(opcode, a),
            (Obj::Keyboard, _) => self.keyboard_event(opcode, a)?,
            (Obj::DataDevice, 0) => {
                let offer = a.u();
                self.objs.insert(offer, Obj::Offer);
                self.offers.insert(offer, Vec::new());
            }
            (Obj::DataDevice, 1) => {
                // Something is dragged over us; we take no drops.
                let (_serial, _surface, _x, _y) = (a.u(), a.u(), a.u(), a.u());
                self.drop_offer = a.u();
            }
            (Obj::DataDevice, 2) => {
                let offer = std::mem::take(&mut self.drop_offer);
                self.forget_offer(offer);
            }
            (Obj::DataDevice, 5) => {
                let offer = a.u();
                let old = std::mem::replace(&mut self.selection, offer);
                if old != offer {
                    self.forget_offer(old);
                }
            }
            (Obj::Offer, 0) => {
                let mime = a.string();
                if let Some(formats) = self.offers.get_mut(&object) {
                    formats.push(mime);
                }
            }
            (Obj::ClipSource | Obj::DragSource | Obj::DeadSource, 1) => {
                // Someone asks for our data in a format, on a pipe.
                let mime = a.string();
                let Some(fd) = self.w.fds.pop_front() else { return Ok(()) };
                match kind {
                    Obj::ClipSource => write_later(fd, data_for(&mime, &self.copied, self.copied_files)),
                    Obj::DragSource => write_later(fd, data_for(&mime, &self.drag_paths, true)),
                    _ => {}
                }
            }
            (Obj::ClipSource, 2) => {
                // Someone else copied.
                self.w.send(object, 1, &[]);
                self.objs.insert(object, Obj::DeadSource);
                if self.clip_source == object {
                    self.clip_source = 0;
                }
            }
            (Obj::DragSource, 0) => {
                let mime = a.string();
                if let Some(drag) = self.drag.as_mut() {
                    drag.accepted = !mime.is_empty();
                }
                self.drag_status();
            }
            (Obj::DragSource, 5) => {
                if let Some(drag) = self.drag.as_mut() {
                    drag.action = a.u();
                }
                self.drag_status();
            }
            (Obj::DragSource, 3) => {
                let moves = self.drag.as_ref().is_some_and(|d| d.action == 2);
                self.pending.push_back(Event::DragDropped { moves });
            }
            (Obj::DragSource, 2 | 4) => {
                if opcode == 4 {
                    self.pending.push_back(Event::DragFinished);
                }
                self.end_drag();
                self.pending.push_back(Event::DragEnded);
            }
            _ => {}
        }
        Ok(())
    }

    fn forget_offer(&mut self, offer: u32) {
        if offer != 0 && self.offers.remove(&offer).is_some() {
            self.objs.remove(&offer);
            self.w.send(offer, 2, &[]); // destroy
        }
    }

    fn pointer_event(&mut self, opcode: u16, mut a: Args) {
        let scale = self.scale as f64;
        let px = |v: f64| (v * scale).floor() as i32;
        match opcode {
            0 => {
                let (serial, surface) = (a.u(), a.u());
                self.serial = serial;
                self.pointer_focus = self.win_of_surface(surface);
                self.pointer_at = (px(a.fixed()), px(a.fixed()));
                self.enter_serial = serial;
                self.set_cursor(serial);
                if let Some(win) = self.pointer_focus {
                    let (x, y) = self.pointer_at;
                    self.pending.push_back(Event::Motion { win, x, y, mods: self.mods() });
                }
            }
            1 => self.pointer_focus = None,
            2 => {
                let _time = a.u();
                self.pointer_at = (px(a.fixed()), px(a.fixed()));
                if let Some(win) = self.pointer_focus {
                    let (x, y) = self.pointer_at;
                    self.pending.push_back(Event::Motion { win, x, y, mods: self.mods() });
                }
            }
            3 => {
                let (serial, time, button, pressed) = (a.u(), a.u(), a.u(), a.u() == 1);
                self.serial = serial;
                let Some(win) = self.pointer_focus else { return };
                // Linux button codes: left 0x110, right 0x111, middle 0x112.
                let button = match button {
                    0x110 => 1,
                    0x111 => 3,
                    0x112 => 2,
                    _ => return,
                };
                if pressed {
                    (self.press_serial, self.press_win) = (serial, win);
                    let (x, y) = self.pointer_at;
                    self.pending.push_back(Event::Press { win, button, x, y, time, mods: self.mods() });
                } else {
                    self.pending.push_back(Event::Release { win, button });
                }
            }
            4 => {
                let (_time, axis, value) = (a.u(), a.u(), a.fixed());
                if axis == 0 {
                    self.wheel_distance += value;
                }
                if self.seat_version < 5 {
                    self.wheel_frame(); // old compositors send no frame event
                }
            }
            5 => self.wheel_frame(),
            8 => {
                let (axis, steps) = (a.u(), a.i());
                if axis == 0 {
                    self.wheel_steps += steps;
                }
            }
            _ => {}
        }
    }

    /// Turns the wheel movement of one pointer frame into lines.
    fn wheel_frame(&mut self) {
        let (steps, distance) = (std::mem::take(&mut self.wheel_steps), std::mem::take(&mut self.wheel_distance));
        let lines = if steps != 0 {
            steps * 3 // a wheel click
        } else {
            // A touchpad: about one line per 5 units of movement.
            self.wheel_rest += distance;
            let lines = (self.wheel_rest / 5.0).trunc();
            self.wheel_rest -= lines * 5.0;
            lines as i32
        };
        if let (Some(win), true) = (self.pointer_focus, lines != 0) {
            self.pending.push_back(Event::Scroll { win, lines });
        }
    }

    fn keyboard_event(&mut self, opcode: u16, mut a: Args) -> io::Result<()> {
        match opcode {
            0 => {
                let (format, size) = (a.u(), a.u() as usize);
                let Some(fd) = self.w.fds.pop_front() else { return Ok(()) };
                if format == 1 {
                    // The XKB keymap as text.
                    let mut text = vec![0u8; size];
                    let file = File::from(fd);
                    let mut got = 0;
                    while got < size {
                        match file.read_at(&mut text[got..], got as u64) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                    }
                    self.keymap = Keymap::parse(&String::from_utf8_lossy(&text[..got]));
                    if debug::enabled() {
                        let sample = |mods| (9..62).map(|code| format!("{:?}", self.keymap.key(code, mods, 0))).collect::<Vec<_>>().join(" ");
                        debug::log(format!("wayland: keymap of {got} bytes\n  plain: {}\n  shift: {}", sample(0), sample(xkb::SHIFT)));
                    }
                }
            }
            1 => {
                let (serial, surface) = (a.u(), a.u());
                self.serial = serial;
                self.keyboard_focus = self.win_of_surface(surface);
                debug::log(format!("wayland: keyboard focus on {:?}", self.keyboard_focus));
            }
            2 => {
                self.keyboard_focus = None;
                self.repeating = None;
            }
            3 => {
                let (serial, _time, key, pressed) = (a.u(), a.u(), a.u(), a.u() == 1);
                self.serial = serial;
                let code = key + 8; // XKB key codes are the kernel's plus 8
                let Some(win) = self.keyboard_focus else { return Ok(()) };
                if !pressed {
                    if self.repeating.is_some_and(|(repeating, ..)| repeating == code) {
                        self.repeating = None;
                    }
                    return Ok(());
                }
                let key = self.keymap.key(code, self.mods, self.group);
                if key != Key::None {
                    self.pending.push_back(Event::Key { win, key, mods: self.mods() });
                    // The compositor leaves repeating a held key to us.
                    self.repeating = (self.repeat_rate > 0).then(|| (code, win, Instant::now() + Duration::from_millis(self.repeat_delay.max(1) as u64)));
                }
            }
            4 => {
                let (_serial, pressed, latched, locked) = (a.u(), a.u(), a.u(), a.u());
                self.mods = pressed | latched | locked;
                self.group = a.u();
            }
            5 => (self.repeat_rate, self.repeat_delay) = (a.i(), a.i()),
            _ => {}
        }
        Ok(())
    }

    /// Shows the wanted cursor over the main window, the arrow elsewhere.
    fn set_cursor(&mut self, serial: u32) {
        if self.cursor_device != 0 {
            // wp_cursor_shape_device_v1 shapes, named as in CSS.
            let shape = match self.cursor {
                _ if !matches!(self.pointer_focus, Some(Win::Main(_))) => 1, // default
                Cursor::Arrow => 1,
                Cursor::Text => 9,
                // nw, n, ne, e, se, s, sw, w
                Cursor::Resize(edge) => [21, 19, 20, 18, 23, 22, 24, 25][edge as usize % 8],
            };
            self.w.send(self.cursor_device, 1, &[U(serial), U(shape)]); // set_shape
            return;
        }
        // No cursor-shape protocol: a small arrow of our own.
        let whole = self.whole_scale();
        if self.cursor_surface == 0 {
            const ARROW: [&str; 17] = [
                "X          ",
                "XX         ",
                "X.X        ",
                "X..X       ",
                "X...X      ",
                "X....X     ",
                "X.....X    ",
                "X......X   ",
                "X.......X  ",
                "X........X ",
                "X.....XXXXX",
                "X..X..X    ",
                "X.X X..X   ",
                "XX  X..X   ",
                "X    X..X  ",
                "     X..X  ",
                "      XX   ",
            ];
            let (w, h) = (12 * whole, ARROW.len() * whole);
            let Ok(buffer) = self.new_buffer(w, h, 0) else { return }; // ARGB8888
            let mut px = vec![0u8; w * h * 4];
            for (y, row) in px.chunks_exact_mut(w * 4).enumerate() {
                for (x, p) in row.chunks_exact_mut(4).enumerate() {
                    match ARROW[y / whole].as_bytes().get(x / whole) {
                        Some(b'X') => p.copy_from_slice(&[0, 0, 0, 255]),
                        Some(b'.') => p.copy_from_slice(&[255; 4]),
                        _ => {}
                    }
                }
            }
            if buffer.file.write_all_at(&px, 0).is_err() {
                return;
            }
            self.cursor_surface = self.w.new_id();
            self.w.send(self.compositor, 0, &[U(self.cursor_surface)]);
            self.w.send(self.cursor_surface, 8, &[I(whole as i32)]);
            self.w.send(self.cursor_surface, 1, &[U(buffer.id), I(0), I(0)]);
            self.w.send(self.cursor_surface, 6, &[]);
            // The buffer lives as long as the connection.
            self.objs.remove(&buffer.id);
            std::mem::forget(buffer.file);
        }
        self.w.send(self.pointer, 0, &[U(serial), U(self.cursor_surface), I(0), I(0)]);
    }

    // ---- dragging out -----------------------------------------------------

    fn drag_status(&mut self) {
        let Some(drag) = self.drag.as_ref() else { return };
        // Old compositors report no action: a drop there copies.
        let accepted = drag.accepted && (self.data_version < 3 || drag.action != 0);
        self.pending.push_back(Event::DragStatus { accepted, moves: drag.action == 2 });
    }

    fn end_drag(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        self.w.send(drag.source, 1, &[]); // destroy
        self.objs.insert(drag.source, Obj::DeadSource);
        for id in [drag.icon_viewport, drag.icon] {
            if id != 0 {
                self.w.send(id, 0, &[]);
            }
        }
        if let Some(buffer) = drag.icon_buffer {
            self.objs.remove(&buffer.id);
            self.w.send(buffer.id, 0, &[]);
        }
    }
}

impl Backend for Wayland {
    fn name(&self) -> &'static str {
        "Wayland"
    }

    fn scale(&mut self) -> f32 {
        self.scale
    }

    fn next_event(&mut self, wait: Option<Duration>) -> io::Result<Option<Event>> {
        let deadline = wait.map(|w| Instant::now() + w);
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let now = Instant::now();
            // A held key types again when its time comes.
            if let Some((code, win, at)) = self.repeating {
                if now >= at {
                    // Read what the compositor sent first: when handling a key
                    // takes longer than the repeat interval, the release would
                    // otherwise never be seen and the key would repeat forever.
                    while self.step(Some(Duration::ZERO))? {}
                    if !self.pending.is_empty() || self.repeating.is_none_or(|(held, ..)| held != code) {
                        continue;
                    }
                    let now = Instant::now();
                    let key = self.keymap.key(code, self.mods, self.group);
                    let interval = Duration::from_millis((1000 / self.repeat_rate.max(1)).max(1) as u64);
                    self.repeating = Some((code, win, now + interval));
                    if key != Key::None {
                        return Ok(Some(Event::Key { win, key, mods: self.mods() }));
                    }
                    continue;
                }
            }
            let mut timeout = deadline.map(|d| d.saturating_duration_since(now));
            if let Some((.., at)) = self.repeating {
                let until = at.saturating_duration_since(now);
                timeout = Some(timeout.map_or(until, |t| t.min(until)));
            }
            if !self.step(timeout)? && deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
        }
    }

    fn present(&mut self, win: Win, cv: &Canvas) -> io::Result<()> {
        if win == FIRST && self.hidden {
            return Ok(()); // taken down: nothing to draw on
        }
        let Some(mut window) = self.window_mut(win).map(std::mem::take) else { return Ok(()) };
        if window.surface == 0 {
            return Ok(());
        }
        let result = self.present_window(&mut window, cv);
        if let Some(slot) = self.window_mut(win) {
            *slot = window;
        }
        result?;
        self.w.flush()
    }

    fn set_own_title_bar(&mut self, own: bool) -> bool {
        if self.main.decoration == 0 {
            return true; // this compositor draws no title bars: it is ours to draw
        }
        self.own_title = own;
        for win in self.mains() {
            if let Some(decoration) = self.window(win).map(|w| w.decoration).filter(|&d| d != 0) {
                self.w.send(decoration, 1, &[U(if own { 1 } else { 2 })]); // set_mode
            }
        }
        own
    }

    fn open_window(&mut self) -> io::Result<u8> {
        let n = (1..self.others.len()).find(|&n| self.others[n].is_none()).unwrap_or(self.others.len());
        if n > u8::MAX as usize {
            return Err(err("too many windows"));
        }
        let win = Win::Main(n as u8);
        let title = self.title.clone();
        let mut window = self.create_window(win, &title);
        if window.decoration != 0 {
            self.w.send(window.decoration, 1, &[U(if self.own_title { 1 } else { 2 })]); // set_mode
        }
        self.w.send(window.toplevel, 8, &[I(360), I(200)]); // set_min_size
        // As big as the first window.
        (window.lw, window.lh) = (self.main.lw, self.main.lh);
        self.w.send(window.surface, 6, &[]); // commit: ask for the first configure
        if self.others.len() <= n {
            self.others.resize_with(n + 1, || None);
        }
        self.others[n] = Some(window);
        while self.window(win).is_some_and(|w| !w.configured) {
            self.step(None)?;
        }
        Ok(n as u8)
    }

    fn close_window(&mut self, n: u8) {
        let Some(window) = self.others.get_mut(n as usize).and_then(Option::take) else { return };
        self.destroy_window(window);
        for focus in [&mut self.pointer_focus, &mut self.keyboard_focus] {
            if *focus == Some(Win::Main(n)) {
                *focus = None;
            }
        }
        if self.press_win == Win::Main(n) {
            self.press_win = FIRST;
        }
        if let Some(size) = self.announced.get_mut(n as usize) {
            *size = (0, 0);
        }
        self.repeating = None;
        let _ = self.w.flush();
    }

    fn select(&mut self, n: u8) {
        self.selected = n;
    }

    fn dialog_needs_title_bar(&self) -> bool {
        self.decorations == 0
    }

    fn start_move(&mut self) {
        if let Some(window) = self.window(self.press_win) {
            self.w.send(window.toplevel, 5, &[U(self.seat), U(self.press_serial)]);
        }
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        if std::mem::replace(&mut self.cursor, cursor) != cursor && matches!(self.pointer_focus, Some(Win::Main(_))) {
            self.set_cursor(self.enter_serial);
        }
    }

    fn start_resize(&mut self, edge: u32) {
        // From "0 top-left, then clockwise" to xdg_toplevel's edge bits.
        let edges = [5, 1, 9, 8, 10, 2, 6, 4][edge as usize % 8];
        if let Some(toplevel) = self.window(self.press_win).map(|w| w.toplevel) {
            self.w.send(toplevel, 6, &[U(self.seat), U(self.press_serial), U(edges)]);
        }
    }

    fn hide_to_tray(&mut self) -> bool {
        // Wayland has no hidden windows: take it down, keeping its size,
        // and make it again when asked.
        self.close_dialog();
        let main = std::mem::take(&mut self.main);
        (self.main.lw, self.main.lh) = (main.lw, main.lh);
        self.destroy_window(main);
        for focus in [&mut self.pointer_focus, &mut self.keyboard_focus] {
            *focus = None;
        }
        self.repeating = None;
        self.hidden = true;
        let _ = self.w.flush();
        true
    }

    fn show(&mut self) {
        if !std::mem::replace(&mut self.hidden, false) {
            return;
        }
        let (lw, lh) = (self.main.lw, self.main.lh);
        let title = self.title.clone();
        self.main = self.create_window(FIRST, &title);
        if self.main.decoration != 0 {
            self.w.send(self.main.decoration, 1, &[U(if self.own_title { 1 } else { 2 })]); // set_mode
        }
        self.w.send(self.main.toplevel, 8, &[I(360), I(200)]); // set_min_size
        (self.main.lw, self.main.lh) = (lw, lh);
        self.w.send(self.main.surface, 6, &[]); // commit: ask for the first configure
        while !self.main.configured {
            if self.step(None).is_err() {
                return;
            }
        }
        self.pending.push_back(Event::Redraw(FIRST));
    }

    fn minimize(&mut self) {
        if let Some(toplevel) = self.window(Win::Main(self.selected)).map(|w| w.toplevel) {
            self.w.send(toplevel, 13, &[]);
        }
    }

    fn toggle_maximize(&mut self) {
        if let Some((toplevel, maximized)) = self.window(Win::Main(self.selected)).map(|w| (w.toplevel, w.maximized)) {
            self.w.send(toplevel, if maximized { 10 } else { 9 }, &[]);
        }
    }

    fn copy(&mut self, text: &str, file: bool) -> bool {
        if self.data_device == 0 {
            return false;
        }
        if self.clip_source != 0 {
            self.w.send(self.clip_source, 1, &[]); // destroy the previous copy
            self.objs.insert(self.clip_source, Obj::DeadSource);
        }
        let source = self.w.new_id();
        self.w.send(self.data_manager, 0, &[U(source)]); // create_data_source
        self.objs.insert(source, Obj::ClipSource);
        let files: &[&str] = if file { &[MIME_URIS, MIME_GNOME_FILES] } else { &[] };
        for mime in files.iter().chain(&MIME_TEXT) {
            self.w.send(source, 0, &[S(mime)]); // offer
        }
        self.w.send(self.data_device, 1, &[U(source), U(self.serial)]); // set_selection
        self.clip_source = source;
        self.copied = text.to_string();
        self.copied_files = file;
        self.w.flush().is_ok()
    }

    fn request_paste(&mut self) {
        if self.clip_source != 0 {
            // Our own copy: no need to ask the compositor for it.
            self.pending.push_back(Event::Paste(self.copied.clone()));
            return;
        }
        let Some(formats) = self.offers.get(&self.selection) else { return };
        let Some(mime) = MIME_TEXT.iter().find(|m| formats.iter().any(|f| f == *m)) else { return };
        let Ok((mut reader, writer)) = io::pipe() else { return };
        self.w.send(self.selection, 1, &[S(mime), Fd(writer.as_raw_fd())]); // receive
        drop(writer);
        if self.w.flush().is_err() {
            return;
        }
        // The owner writes at its own pace: give it a second.
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = reader.read_to_end(&mut data);
            let _ = done.send(data);
        });
        if let Ok(data) = result.recv_timeout(Duration::from_secs(1)) {
            self.pending.push_back(Event::Paste(String::from_utf8_lossy(&data).into_owned()));
        }
    }

    /// A Wayland clipboard lives only as long as the program that owns it.
    /// So the connection itself is handed to a small background process,
    /// which keeps answering for our copy until someone else copies.
    fn keep_clipboard(&mut self) {
        if self.clip_source == 0 || self.copied.is_empty() {
            return;
        }
        self.close_dialog();
        let main = std::mem::take(&mut self.main);
        self.destroy_window(main);
        for n in 1..self.others.len() {
            if let Some(window) = self.others[n].take() {
                self.destroy_window(window);
            }
        }
        if self.w.flush().is_err() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let Ok(sock) = self.w.sock.try_clone() else { return };
        // What the helper needs to know, readable on its standard output.
        let mut state = format!("{} {} {} {} {}\n", self.clip_source, self.wm_base, self.keyboard, self.copied_files as u8, self.w.inb.len()).into_bytes();
        state.extend(&self.w.inb);
        state.extend(self.copied.as_bytes());
        let Ok(file) = memory_file(state.len()) else { return };
        if file.write_all_at(&state, 0).is_err() {
            return;
        }
        let _ = std::process::Command::new(exe)
            .arg("--hold-wayland-clipboard")
            .stdin(std::process::Stdio::from(OwnedFd::from(sock)))
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    fn open_dialog(&mut self, w: i32, h: i32, title: &str) -> io::Result<()> {
        self.close_dialog();
        let mut dialog = self.create_window(Win::Dialog, title);
        dialog.lw = (w as f32 / self.scale).ceil().max(1.0) as i32;
        dialog.lh = (h as f32 / self.scale).ceil().max(1.0) as i32;
        let parent = self.window(Win::Main(self.selected)).map_or(self.main.toplevel, |w| w.toplevel);
        self.w.send(dialog.toplevel, 1, &[U(parent)]); // set_parent
        self.w.send(dialog.toplevel, 8, &[I(dialog.lw), I(dialog.lh)]); // set_min_size
        self.w.send(dialog.toplevel, 7, &[I(dialog.lw), I(dialog.lh)]); // set_max_size: fixed
        self.w.send(dialog.surface, 6, &[]); // commit
        self.dialog = Some(dialog);
        // It can be drawn once the compositor has configured it.
        while self.dialog.as_ref().is_some_and(|d| !d.configured) {
            self.step(None)?;
        }
        Ok(())
    }

    fn close_dialog(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            self.destroy_window(dialog);
            for focus in [&mut self.pointer_focus, &mut self.keyboard_focus] {
                if *focus == Some(Win::Dialog) {
                    *focus = None;
                }
            }
            self.repeating = None;
            let _ = self.w.flush();
        }
    }

    fn start_drag(&mut self, paths: &str, image: &Canvas, copy: bool) -> io::Result<()> {
        let Some(origin) = self.window(self.press_win).map(|w| w.surface) else { return Ok(()) };
        if self.data_device == 0 || self.drag.is_some() {
            return Ok(());
        }
        let source = self.w.new_id();
        self.w.send(self.data_manager, 0, &[U(source)]); // create_data_source
        self.objs.insert(source, Obj::DragSource);
        for mime in [MIME_URIS, MIME_TEXT[0], MIME_TEXT[2]] {
            self.w.send(source, 0, &[S(mime)]);
        }
        if self.data_version >= 3 {
            // Copy (1) and move (2): the target chooses, as file managers
            // do (moving within a disk). With Ctrl held, copy only.
            self.w.send(source, 2, &[U(if copy { 1 } else { 3 })]); // set_actions
        }
        let icon = self.w.new_id();
        self.w.send(self.compositor, 0, &[U(icon)]);
        self.w.send(self.data_device, 0, &[U(source), U(origin), U(icon), U(self.press_serial)]); // start_drag
        let mut drag = Drag { source, icon, icon_viewport: 0, icon_buffer: None, accepted: false, action: 0 };
        if image.w > 0 && image.h > 0 {
            let buffer = self.new_buffer(image.w, image.h, 1)?;
            Self::fill_buffer(&buffer, image)?;
            // The image hangs below and right of the pointer.
            self.w.send(icon, 1, &[U(buffer.id), I(14), I(10)]); // attach
            if self.viewporter != 0 {
                drag.icon_viewport = self.w.new_id();
                self.w.send(self.viewporter, 1, &[U(drag.icon_viewport), U(icon)]);
                let logical = |v: usize| ((v as f32 / self.scale).ceil() as i32).max(1);
                self.w.send(drag.icon_viewport, 2, &[I(logical(image.w)), I(logical(image.h))]);
            }
            self.w.send(icon, 2, &[I(0), I(0), I(i32::MAX), I(i32::MAX)]);
            self.w.send(icon, 6, &[]);
            drag.icon_buffer = Some(buffer);
        }
        self.drag = Some(drag);
        self.drag_paths = paths.to_string();
        self.w.flush()
    }

    fn drag_wants_copy(&mut self, _copy: bool) {
        // During a drag the compositor owns the pointer and the modifier
        // keys: it settles copy or move with the target by itself.
    }
}

/// The body of `ff-gui --hold-wayland-clipboard`: takes over the closed
/// window's connection (its standard input) and keeps serving what was
/// copied until another program copies something.
pub fn hold_clipboard() -> io::Result<()> {
    // SAFETY: the window started us with these two descriptors and
    // nothing else in this process uses them.
    let (state, sock) = unsafe { (File::from_raw_fd(1), UnixStream::from_raw_fd(0)) };
    let mut bytes = vec![0u8; state.metadata()?.len() as usize];
    state.read_exact_at(&mut bytes, 0)?;
    let split = bytes.iter().position(|&b| b == b'\n').ok_or_else(|| err("bad state"))?;
    let numbers: Vec<usize> = String::from_utf8_lossy(&bytes[..split]).split(' ').filter_map(|n| n.parse().ok()).collect();
    let [source, wm_base, keyboard, files, unread] = numbers[..] else { return Err(err("bad state")) };
    let rest = &bytes[split + 1..];
    let text = String::from_utf8_lossy(rest.get(unread..).unwrap_or_default()).into_owned();
    let mut w = Wire::new(sock);
    w.inb = rest[..unread.min(rest.len())].to_vec();
    loop {
        w.flush()?;
        let Some((object, opcode, body)) = w.read(None)? else { continue };
        let mut a = Args(&body);
        match (object as usize, opcode) {
            (1, 0) => return Err(err("Wayland protocol error")),
            (o, 0) if o == wm_base => w.send(object, 3, &[U(a.u())]), // ping -> pong
            (o, 0) if o == keyboard => drop(w.fds.pop_front()),       // a keymap we do not need
            (o, 1) if o == source => {
                let mime = a.string();
                if let Some(fd) = w.fds.pop_front() {
                    write_later(fd, data_for(&mime, &text, files == 1));
                }
            }
            (o, 2) if o == source => return Ok(()), // someone else copied
            _ => {}
        }
    }
}
