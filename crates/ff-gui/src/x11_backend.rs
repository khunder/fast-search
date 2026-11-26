//! The X11 backend: the application's windows, input, clipboard and
//! drag and drop over the raw X protocol (see `x11`). It also serves
//! Wayland sessions through XWayland when the native backend is not used.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crate::backend::{Backend, Cursor, Event, FIRST, Mods, Win, WindowSpec};
use crate::canvas::{Canvas, Icon};
use crate::debug;
use crate::x11::P::*;
use crate::x11::*;

/// The atoms used to talk about the clipboard.
#[derive(Clone, Copy)]
struct ClipAtoms {
    clipboard: u32,
    targets: u32,
    utf8_string: u32,
    /// Plain text under its MIME names, which Wayland programs ask for.
    text_plain_utf8: u32,
    text_plain: u32,
    /// "text/uri-list": the standard format for copied files.
    uri_list: u32,
    /// GNOME's own format for copied files.
    gnome_files: u32,
}

impl ClipAtoms {
    fn intern(c: &mut Conn) -> io::Result<ClipAtoms> {
        Ok(ClipAtoms {
            clipboard: c.intern_atom("CLIPBOARD")?,
            targets: c.intern_atom("TARGETS")?,
            utf8_string: c.intern_atom("UTF8_STRING")?,
            text_plain_utf8: c.intern_atom("text/plain;charset=utf-8")?,
            text_plain: c.intern_atom("text/plain")?,
            uri_list: c.intern_atom("text/uri-list")?,
            gnome_files: c.intern_atom("x-special/gnome-copied-files")?,
        })
    }
}

/// The atoms of the XDND drag-and-drop protocol.
#[derive(Clone, Copy)]
struct DndAtoms {
    aware: u32,
    proxy: u32,
    selection: u32,
    type_list: u32,
    enter: u32,
    position: u32,
    status: u32,
    leave: u32,
    drop: u32,
    finished: u32,
    action_copy: u32,
    action_move: u32,
}

impl DndAtoms {
    fn intern(c: &mut Conn) -> io::Result<DndAtoms> {
        Ok(DndAtoms {
            aware: c.intern_atom("XdndAware")?,
            proxy: c.intern_atom("XdndProxy")?,
            selection: c.intern_atom("XdndSelection")?,
            type_list: c.intern_atom("XdndTypeList")?,
            enter: c.intern_atom("XdndEnter")?,
            position: c.intern_atom("XdndPosition")?,
            status: c.intern_atom("XdndStatus")?,
            leave: c.intern_atom("XdndLeave")?,
            drop: c.intern_atom("XdndDrop")?,
            finished: c.intern_atom("XdndFinished")?,
            action_copy: c.intern_atom("XdndActionCopy")?,
            action_move: c.intern_atom("XdndActionMove")?,
        })
    }
}

/// The window a drag is currently over.
struct DropTarget {
    window: u32,
    /// Where messages for it go (itself, or its proxy).
    dest: u32,
    /// Protocol version we speak with it.
    version: u32,
    /// It said it would take the drop here.
    accepted: bool,
    /// What it would do with the files: move them (else copy).
    moves: bool,
    /// We sent a position and it has not answered yet.
    waiting: bool,
    /// The newest position, held back until it answers.
    pending: Option<(i32, i32, u32)>,
}

/// A drag in progress.
struct Drag {
    /// Ctrl is held: ask for a copy instead of the default move.
    copy: bool,
    target: Option<DropTarget>,
    /// The window showing the image that follows the pointer, its
    /// pixels (to repaint it) and their width.
    ghost: u32,
    ghost_px: Vec<u8>,
    ghost_w: usize,
    /// Windows a Wayland compositor uses to receive drops on behalf of
    /// native Wayland programs.
    proxies: Vec<u32>,
    motions: u32,
}

/// Answers another application's request (a SelectionRequest event) for
/// data we own. `file` says the text is paths of files, one per line,
/// which are then also offered in the formats file managers take.
fn serve_selection(c: &mut Conn, ev: &[u8], text: &str, file: bool, atoms: &ClipAtoms) {
    let ClipAtoms { targets, utf8_string, text_plain_utf8, text_plain, uri_list, gnome_files, .. } = *atoms;
    let uris: Vec<String> = text.lines().map(|p| ff_core::desktop::file_uri(std::path::Path::new(p))).collect();
    let (requestor, selection, target) = (u32at(ev, 12), u32at(ev, 16), u32at(ev, 20));
    let mut property = match u32at(ev, 24) {
        0 => target, // obsolete clients
        p => p,
    };
    if target == targets {
        let mut list = vec![targets, utf8_string, text_plain_utf8, text_plain, ATOM_STRING];
        if file {
            list.extend([uri_list, gnome_files]);
        }
        let list: Vec<u8> = list.iter().flat_map(|a| a.to_le_bytes()).collect();
        c.set_property(requestor, property, ATOM_ATOM, 32, &list);
    } else if file && target == uri_list {
        c.set_property(requestor, property, target, 8, (uris.join("\r\n") + "\r\n").as_bytes());
    } else if file && target == gnome_files {
        c.set_property(requestor, property, target, 8, format!("copy\n{}", uris.join("\n")).as_bytes());
    } else if [utf8_string, text_plain_utf8, text_plain, ATOM_STRING].contains(&target) {
        c.set_property(requestor, property, target, 8, text.as_bytes());
    } else {
        property = 0; // refused
    }
    // SendEvent(SelectionNotify) back to the requestor.
    let event = [
        W(requestor),
        W(0),
        B(EV_SELECTION_NOTIFY),
        B(0),
        H(0),
        W(u32at(ev, 4)),
        W(requestor),
        W(selection),
        W(target),
        W(property),
        S(&[0; 8]),
    ];
    c.req(25, 0, &event);
}

/// Takes ownership of the clipboard for `window`; true if we got it.
fn own_clipboard(c: &mut Conn, window: u32, clipboard: u32) -> io::Result<bool> {
    // Time 0 means "now". A key-press timestamp can be refused: under
    // XWayland it comes from the compositor's clock, not the X server's.
    c.req(22, 0, &[W(window), W(clipboard), W(0)]); // SetSelectionOwner
    let seq = c.req(23, 0, &[W(clipboard)]); // GetSelectionOwner
    Ok(c.reply(seq)?.is_some_and(|r| u32at(&r, 8) == window))
}

/// `--hold-clipboard text|file`: keeps text (read from stdin) on the
/// clipboard after the window that copied it has closed, until something
/// else is copied. X11 clipboards live in the owning program, so without
/// this a copy would vanish the moment the window closes.
pub fn hold_clipboard(file: bool) -> io::Result<()> {
    let mut text = String::new();
    io::Read::read_to_string(&mut io::stdin(), &mut text)?;
    let mut c = Conn::connect()?;
    let win = c.new_id();
    // An invisible 1x1 window, never mapped, just to own the selection.
    c.req(1, 0, &[W(win), W(c.root), H(0), H(0), H(1), H(1), H(0), H(1), W(0), W(0)]);
    let atoms = ClipAtoms::intern(&mut c)?;
    if !own_clipboard(&mut c, win, atoms.clipboard)? {
        return Ok(());
    }
    loop {
        let Some(ev) = c.next_event(None)? else { continue };
        match ev[0] & 0x7f {
            EV_SELECTION_REQUEST => serve_selection(&mut c, &ev, &text, file, &atoms),
            EV_SELECTION_CLEAR => return Ok(()), // someone else copied
            _ => {}
        }
    }
}

pub struct X11 {
    c: Conn,
    /// The first search window: it owns the clipboard and offers drags.
    win: u32,
    /// Every search window by number (the first is `win`); 0 where one
    /// was closed.
    wins: Vec<u32>,
    /// The one `select` chose.
    selected: usize,
    /// The window title, and whether we draw title bars ourselves, for
    /// windows opened later.
    title: String,
    own_title: bool,
    /// The pointer shape shown, the standard cursor font (0 until it
    /// is needed) and the cursors made from it so far.
    cursor: Cursor,
    cursor_font: u32,
    cursors: Vec<(Cursor, u32)>,
    gc: u32,
    /// The dialog window, or 0.
    dialog: u32,
    /// What each search window currently shows, by number, and the
    /// dialog (empty: must resend everything).
    shown: Vec<Vec<u8>>,
    dialog_shown: Vec<u8>,
    scale: f32,
    /// Events produced but not yet handed out.
    pending: VecDeque<Event>,
    /// Each search window's size, by number.
    sizes: Vec<(usize, usize)>,

    wm_delete: u32,
    wm_protocols: u32,
    net_wm_name: u32,
    net_wm_icon: u32,
    paste_prop: u32,
    motif_hints: u32,
    net_wm_moveresize: u32,
    net_wm_state: u32,
    net_wm_maximized: [u32; 2],
    wm_change_state: u32,
    net_active_window: u32,
    clip: ClipAtoms,
    dnd: DndAtoms,

    copied: String,
    copied_file: bool,
    owns_clipboard: bool,
    /// Pointer position on the screen at the last button press.
    pointer_root: (i32, i32),
    drag: Option<Drag>,
    /// Paths (one per line) offered to whatever a drag is dropped on.
    dropped: String,
}

/// The atoms a search window's properties need.
#[derive(Clone, Copy)]
struct WindowAtoms {
    wm_protocols: u32,
    wm_delete: u32,
    net_wm_name: u32,
    net_wm_icon: u32,
    utf8_string: u32,
}

/// Creates a search window (not mapped yet) with its title, class, close
/// protocol and icon.
fn make_window(c: &mut Conn, atoms: &WindowAtoms, title: &str, w: i32, h: i32) -> u32 {
    let win = c.new_id();
    // Events: KeyPress | ButtonPress | ButtonRelease | PointerMotion |
    // Exposure | StructureNotify.
    let events = 0x1 | 0x4 | 0x8 | 0x40 | 0x8000 | 0x20000;
    c.req(1, 0, &[W(win), W(c.root), H(0), H(0), H(w as u16), H(h as u16), H(0), H(1), W(0), W(0x800), W(events)]);
    c.set_property(win, atoms.wm_protocols, ATOM_ATOM, 32, &atoms.wm_delete.to_le_bytes());
    c.set_property(win, ATOM_WM_NAME, ATOM_STRING, 8, title.as_bytes());
    c.set_property(win, atoms.net_wm_name, atoms.utf8_string, 8, title.as_bytes());
    c.set_property(win, ATOM_WM_CLASS, ATOM_STRING, 8, b"ff-gui\0ff-gui\0");
    // _NET_WM_ICON: width, height, then ARGB pixels, for each size.
    let mut icon = Vec::new();
    for size in [48usize, 128] {
        icon.extend((size as u32).to_le_bytes());
        icon.extend((size as u32).to_le_bytes());
        icon.extend(Icon::App.render(size).into_iter().flatten());
    }
    c.set_property(win, atoms.net_wm_icon, 6, 32, &icon); // type CARDINAL
    win
}

/// Desktop scale from the Xft.dpi resource (96 dpi = 1.0).
fn desktop_scale(c: &mut Conn) -> f32 {
    let dpi = (|| {
        let rm = c.get_property(c.root, 23, false).ok()?; // RESOURCE_MANAGER
        let rm = String::from_utf8_lossy(&rm).into_owned();
        rm.lines().find_map(|l| l.strip_prefix("Xft.dpi:"))?.trim().parse::<f32>().ok()
    })();
    dpi.map_or(1.0, |dpi| dpi / 96.0).clamp(0.5, 8.0)
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl X11 {
    pub fn connect(spec: &WindowSpec) -> io::Result<X11> {
        let mut c = Conn::connect()?;
        let scale = spec.scale.unwrap_or_else(|| desktop_scale(&mut c));
        let u = (scale.round() as i32).max(1);
        let (w, h) = (spec.width * u, spec.height * u);
        let wm_protocols = c.intern_atom("WM_PROTOCOLS")?;
        let wm_delete = c.intern_atom("WM_DELETE_WINDOW")?;
        let net_wm_name = c.intern_atom("_NET_WM_NAME")?;
        let net_wm_icon = c.intern_atom("_NET_WM_ICON")?;
        let clip = ClipAtoms::intern(&mut c)?;
        let atoms = WindowAtoms { wm_protocols, wm_delete, net_wm_name, net_wm_icon, utf8_string: clip.utf8_string };
        let win = make_window(&mut c, &atoms, spec.title, w, h);
        let gc = c.new_id();
        c.req(55, 0, &[W(gc), W(win), W(0x10000), W(0)]); // no graphics exposures

        let mut x = X11 {
            win,
            wins: vec![win],
            selected: 0,
            title: spec.title.to_string(),
            own_title: false,
            cursor: Cursor::Arrow,
            cursor_font: 0,
            cursors: Vec::new(),
            gc,
            dialog: 0,
            shown: vec![Vec::new()],
            dialog_shown: Vec::new(),
            scale,
            pending: VecDeque::from([Event::Resize(FIRST, w as usize, h as usize)]),
            sizes: vec![(w as usize, h as usize)],
            wm_delete,
            wm_protocols,
            net_wm_name,
            net_wm_icon,
            paste_prop: c.intern_atom("FF_PASTE")?,
            motif_hints: c.intern_atom("_MOTIF_WM_HINTS")?,
            net_wm_moveresize: c.intern_atom("_NET_WM_MOVERESIZE")?,
            net_wm_state: c.intern_atom("_NET_WM_STATE")?,
            net_wm_maximized: [
                c.intern_atom("_NET_WM_STATE_MAXIMIZED_VERT")?,
                c.intern_atom("_NET_WM_STATE_MAXIMIZED_HORZ")?,
            ],
            wm_change_state: c.intern_atom("WM_CHANGE_STATE")?,
            net_active_window: c.intern_atom("_NET_ACTIVE_WINDOW")?,
            clip,
            dnd: DndAtoms::intern(&mut c)?,
            copied: String::new(),
            copied_file: false,
            owns_clipboard: false,
            pointer_root: (0, 0),
            drag: None,
            dropped: String::new(),
            c,
        };
        // Started at sign-in: not shown until asked for.
        if !spec.hidden {
            x.c.req(8, 0, &[W(win)]); // MapWindow
        }
        Ok(x)
    }

    fn win_of(&self, window: u32) -> Option<Win> {
        if window == 0 {
            None
        } else if window == self.dialog {
            Some(Win::Dialog)
        } else {
            self.wins.iter().position(|&w| w == window).map(|n| Win::Main(n as u8))
        }
    }

    /// The selected search window (the first if it was closed).
    fn cur(&self) -> u32 {
        self.wins.get(self.selected).copied().filter(|&w| w != 0).unwrap_or(self.win)
    }

    fn atoms(&self) -> WindowAtoms {
        WindowAtoms {
            wm_protocols: self.wm_protocols,
            wm_delete: self.wm_delete,
            net_wm_name: self.net_wm_name,
            net_wm_icon: self.net_wm_icon,
            utf8_string: self.clip.utf8_string,
        }
    }

    /// Asks the window manager to draw a title bar for `window` or not.
    fn decorate(&mut self, window: u32) {
        // _MOTIF_WM_HINTS: flags = decorations field is set; decorations 0/1.
        let hints = words(&[2, 0, if self.own_title { 0 } else { 1 }, 0, 0]);
        self.c.set_property(window, self.motif_hints, self.motif_hints, 32, &hints);
    }

    /// Sends a request about the selected window to the window manager
    /// (a ClientMessage to the root).
    fn wm_message(&mut self, kind: u32, data: [u32; 5]) {
        let [a, b, c, d, e] = data;
        let window = self.cur();
        // SendEvent with SubstructureRedirect | SubstructureNotify.
        let event =
            [W(self.c.root), W(0x18_0000), B(EV_CLIENT_MESSAGE), B(32), H(0), W(window), W(kind), W(a), W(b), W(c), W(d), W(e)];
        self.c.req(25, 0, &event);
    }

    /// Turns one raw X event into application events (queued in `pending`).
    fn translate(&mut self, ev: &[u8]) -> io::Result<()> {
        let at = |i| u16at(ev, i) as i16 as i32;
        let code = ev[0] & 0x7f;
        // Input events name their window at offset 12, Expose and
        // ClientMessage at 4, ConfigureNotify at 8.
        let window = match code {
            EV_KEY_PRESS | EV_BUTTON_PRESS | EV_BUTTON_RELEASE | EV_MOTION => u32at(ev, 12),
            EV_EXPOSE | EV_CLIENT_MESSAGE => u32at(ev, 4),
            EV_CONFIGURE => u32at(ev, 8),
            _ => self.win,
        };
        let state = u16at(ev, 28);
        let mods = Mods { shift: state & MOD_SHIFT != 0, ctrl: state & MOD_CTRL != 0, alt: state & MOD_ALT != 0 };
        let Some(win) = self.win_of(window) else {
            // Not one of the two windows: only the drag image can be exposed.
            if code == EV_EXPOSE {
                if let Some(drag) = self.drag.as_ref().filter(|d| d.ghost == window && d.ghost != 0) {
                    self.c.put_image(window, self.gc, drag.ghost_w, 0, &drag.ghost_px)?;
                }
            }
            return Ok(());
        };
        match code {
            EV_EXPOSE => {
                self.shown_of(win).clear();
                self.pending.push_back(Event::Redraw(win));
            }
            EV_CONFIGURE if win != Win::Dialog => {
                let Win::Main(n) = win else { return Ok(()) };
                let size = (u16at(ev, 20) as usize, u16at(ev, 22) as usize);
                if self.sizes.len() <= n as usize {
                    self.sizes.resize(n as usize + 1, (0, 0));
                }
                if size != self.sizes[n as usize] {
                    self.sizes[n as usize] = size;
                    self.shown_of(win).clear();
                    self.pending.push_back(Event::Resize(win, size.0.max(1), size.1.max(1)));
                }
            }
            EV_KEY_PRESS => {
                let key = self.c.key(ev[1], state);
                // A modifier on its own is not a key press for us.
                if !matches!(key, Key::None | Key::Sym(0xffe1..=0xffee)) {
                    self.pending.push_back(Event::Key { win, key, mods });
                }
            }
            EV_BUTTON_PRESS => {
                self.pointer_root = (at(20), at(22));
                match ev[1] {
                    4 => self.pending.push_back(Event::Scroll { win, lines: -3 }),
                    5 => self.pending.push_back(Event::Scroll { win, lines: 3 }),
                    button => {
                        let (x, y, time) = (at(24), at(26), u32at(ev, 4));
                        self.pending.push_back(Event::Press { win, button, x, y, time, mods });
                    }
                }
            }
            EV_BUTTON_RELEASE if ev[1] < 4 => {
                if ev[1] == 1 && self.drag.is_some() {
                    self.drag_end(u32at(ev, 4));
                }
                self.pending.push_back(Event::Release { win, button: ev[1] });
            }
            EV_MOTION if self.drag.is_some() => {
                // While dragging, motion drives the drag, not the window.
                self.drag_motion((at(20), at(22)), u32at(ev, 4), mods.ctrl)?;
            }
            EV_MOTION => self.pending.push_back(Event::Motion { win, x: at(24), y: at(26), mods }),
            EV_SELECTION_REQUEST => self.selection_request(ev),
            // Someone else copied.
            EV_SELECTION_CLEAR if u32at(ev, 12) == self.clip.clipboard => self.owns_clipboard = false,
            EV_SELECTION_NOTIFY if u32at(ev, 20) != 0 => {
                let data = self.c.get_property(self.win, self.paste_prop, true)?;
                self.pending.push_back(Event::Paste(String::from_utf8_lossy(&data).into_owned()));
            }
            EV_CLIENT_MESSAGE if u32at(ev, 12) == self.wm_delete => self.pending.push_back(Event::Close(win)),
            EV_CLIENT_MESSAGE => self.dnd_message(ev),
            EV_MAPPING => self.c.load_keymap()?,
            _ => {}
        }
        Ok(())
    }

    /// What `win` shows now (to send only what changed).
    fn shown_of(&mut self, win: Win) -> &mut Vec<u8> {
        match win {
            Win::Main(n) => {
                if self.shown.len() <= n as usize {
                    self.shown.resize(n as usize + 1, Vec::new());
                }
                &mut self.shown[n as usize]
            }
            Win::Dialog => &mut self.dialog_shown,
        }
    }

    // ---- dragging out (XDND source) -------------------------------------

    /// Answers a request for data we offer: the clipboard, or the files
    /// being dragged.
    fn selection_request(&mut self, ev: &[u8]) {
        let atoms = self.clip;
        if debug::enabled() {
            let (selection, target) = (self.c.atom_name(u32at(ev, 16)), self.c.atom_name(u32at(ev, 20)));
            debug::log(format!("window {:#x} asks for {selection} as {target}", u32at(ev, 12)));
        }
        if u32at(ev, 16) == self.dnd.selection {
            let paths = self.dropped.clone();
            serve_selection(&mut self.c, ev, &paths, true, &atoms);
        } else {
            let text = self.copied.clone();
            serve_selection(&mut self.c, ev, &text, self.copied_file, &atoms);
        }
    }

    /// Sends one XDND message about `target`; our window id goes first.
    fn dnd_send(&mut self, target: &DropTarget, kind: u32, data: [u32; 4]) {
        let [a, b, c, d] = data;
        let event =
            [W(target.dest), W(0), B(EV_CLIENT_MESSAGE), B(32), H(0), W(target.window), W(kind), W(self.win), W(a), W(b), W(c), W(d)];
        self.c.req(25, 0, &event);
    }

    /// If `window` accepts drops, a target for it.
    fn drop_target(&mut self, window: u32) -> io::Result<Option<DropTarget>> {
        let aware = self.c.get_property(window, self.dnd.aware, false)?;
        if aware.len() < 4 || self.wins.contains(&window) {
            return Ok(None);
        }
        let proxy = self.c.get_property(window, self.dnd.proxy, false)?;
        let dest = if proxy.len() >= 4 { u32at(&proxy, 0) } else { window };
        let version = u32at(&aware, 0).min(5);
        Ok(Some(DropTarget { window, dest, version, accepted: false, moves: false, waiting: false, pending: None }))
    }

    /// Top-level windows that accept drops without belonging to any
    /// program's visible window: input-only and unmanaged. A Wayland
    /// compositor shows one over a native Wayland window during a drag.
    fn drop_proxies(&mut self) -> io::Result<Vec<u32>> {
        let mut proxies = Vec::new();
        for window in self.c.children(self.c.root)? {
            if self.wins.contains(&window) || self.c.get_property(window, self.dnd.aware, false)?.len() < 4 {
                continue;
            }
            let kind = self.c.window_kind(window)?;
            if kind.is_some_and(|(input_only, _, unmanaged)| input_only && unmanaged) {
                proxies.push(window);
            }
        }
        debug::log(format!("compositor drop proxies: {proxies:x?}"));
        Ok(proxies)
    }

    /// The drop-accepting window at a screen position: the outermost one
    /// on the way down from the root window.
    fn drop_target_at(&mut self, x: i32, y: i32) -> io::Result<Option<DropTarget>> {
        let root = self.c.root;
        let mut window = root;
        for _ in 0..16 {
            // TranslateCoordinates also names the child window at the point.
            let seq = self.c.req(40, 0, &[W(root), W(window), H(x as u16), H(y as u16)]);
            let child = self.c.reply(seq)?.map_or(0, |r| u32at(&r, 8));
            if child == 0 {
                break;
            }
            window = child;
            if let Some(target) = self.drop_target(window)? {
                return Ok(Some(target));
            }
        }
        Ok(None)
    }

    /// Tells the target where the pointer is and what we would like it to
    /// do with the files: move them, or copy them when `copy` is set.
    fn dnd_send_position(&mut self, target: &mut DropTarget, x: i32, y: i32, time: u32, copy: bool) {
        if target.waiting {
            target.pending = Some((x, y, time)); // one position at a time
            return;
        }
        target.waiting = true;
        let at = ((x as u32 & 0xffff) << 16) | (y as u32 & 0xffff);
        let action = if copy { self.dnd.action_copy } else { self.dnd.action_move };
        self.dnd_send(target, self.dnd.position, [0, at, time, action]);
    }

    /// Pointer moved during a drag.
    fn drag_motion(&mut self, root: (i32, i32), time: u32, ctrl: bool) -> io::Result<()> {
        let Some(mut drag) = self.drag.take() else { return Ok(()) };
        drag.copy = ctrl;
        let mut target = self.drop_target_at(root.0, root.1)?;
        if target.is_none() {
            // Nothing found by position. A compositor's drop proxy that is
            // currently shown means the pointer is over a Wayland program.
            for i in 0..drag.proxies.len() {
                let proxy = drag.proxies[i];
                if self.c.window_kind(proxy)?.is_some_and(|(_, viewable, _)| viewable) {
                    target = self.drop_target(proxy)?;
                    break;
                }
            }
        }
        if drag.ghost != 0 {
            // ConfigureWindow x, y: follow the pointer, just clear of it so
            // the image is never the window under the pointer.
            let u = (self.scale.round() as i32).max(1);
            let (gx, gy) = (root.0 + 14 * u, root.1 + 18 * u);
            self.c.req(12, 0, &[W(drag.ghost), H(1 | 2), H(0), W(gx as u32), W(gy as u32)]);
        }
        drag.motions += 1;
        // Move the drag to a (possibly new) target under the pointer.
        if drag.target.as_ref().map(|t| t.window) != target.as_ref().map(|t| t.window) {
            debug::log(format!("pointer at {root:?}: drop target {:x?}", target.as_ref().map(|t| t.window)));
            if let Some(old) = drag.target.take() {
                self.dnd_send(&old, self.dnd.leave, [0; 4]);
                if old.accepted {
                    self.pending.push_back(Event::DragStatus { accepted: false, moves: false });
                }
            }
            if let Some(new) = &target {
                // Enter: protocol version, a flag saying the full list of
                // formats is in XdndTypeList, and the first three of them.
                let first = [self.clip.uri_list, self.clip.text_plain_utf8, self.clip.text_plain];
                self.dnd_send(new, self.dnd.enter, [new.version << 24 | 1, first[0], first[1], first[2]]);
            }
            drag.target = target;
        }
        if let Some(mut target) = drag.target.take() {
            self.dnd_send_position(&mut target, root.0, root.1, time, drag.copy);
            drag.target = Some(target);
        }
        self.drag = Some(drag);
        Ok(())
    }

    /// Button released: drop on the target if it accepted, else cancel.
    fn drag_end(&mut self, time: u32) {
        let Some(drag) = self.drag.take() else { return };
        if drag.ghost != 0 {
            self.c.req(4, 0, &[W(drag.ghost)]); // DestroyWindow
        }
        match drag.target {
            Some(target) if target.accepted => {
                debug::log(format!("dropped on {:#x} (moves: {})", target.window, target.moves));
                self.dnd_send(&target, self.dnd.drop, [0, time, 0, 0]);
                self.pending.push_back(Event::DragDropped { moves: target.moves });
            }
            Some(target) => self.dnd_send(&target, self.dnd.leave, [0; 4]),
            None => {}
        }
        self.pending.push_back(Event::DragEnded);
    }

    /// A message from a drop target.
    fn dnd_message(&mut self, ev: &[u8]) {
        let (kind, from) = (u32at(ev, 8), u32at(ev, 12));
        if kind == self.dnd.finished {
            debug::log(format!("window {from:#x} finished the drop (success flag {})", u32at(ev, 16) & 1));
            self.pending.push_back(Event::DragFinished);
        }
        if kind != self.dnd.status {
            return;
        }
        // The action the target settled on. (Old targets name none; they copy.)
        let (accept, action) = (u32at(ev, 16) & 1 != 0, u32at(ev, 28));
        let moves = action == self.dnd.action_move;
        let Some(mut drag) = self.drag.take() else { return };
        if let Some(target) = drag.target.as_mut().filter(|t| t.window == from) {
            if (accept, moves) != (target.accepted, target.moves) {
                self.pending.push_back(Event::DragStatus { accepted: accept, moves });
            }
            target.accepted = accept;
            target.moves = moves;
            target.waiting = false;
            if let Some((x, y, time)) = target.pending.take() {
                let (copy, mut t) = (drag.copy, drag.target.take().unwrap());
                self.dnd_send_position(&mut t, x, y, time, copy);
                drag.target = Some(t);
            }
        }
        self.drag = Some(drag);
    }
}

impl Backend for X11 {
    fn name(&self) -> &'static str {
        "X11"
    }

    fn scale(&mut self) -> f32 {
        self.scale
    }

    fn next_event(&mut self, wait: Option<Duration>) -> io::Result<Option<Event>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let Some(raw) = self.c.next_event(wait)? else { return Ok(None) };
            self.translate(&raw)?;
        }
    }

    /// Sends the rows that changed since the last frame to the window.
    fn present(&mut self, win: Win, cv: &Canvas) -> io::Result<()> {
        let window = match win {
            Win::Main(n) => self.wins.get(n as usize).copied().unwrap_or(0),
            Win::Dialog => self.dialog,
        };
        if window == 0 {
            return Ok(());
        }
        let gc = self.gc;
        let shown = std::mem::take(self.shown_of(win));
        let mut shown = shown;
        let stride = cv.w * 4;
        let same_size = shown.len() == cv.px.len();
        let mut row = 0;
        while row < cv.h {
            let changed = |r: usize| !same_size || shown[r * stride..(r + 1) * stride] != cv.px[r * stride..(r + 1) * stride];
            if !changed(row) {
                row += 1;
                continue;
            }
            let start = row;
            while row < cv.h && changed(row) {
                row += 1;
            }
            self.c.put_image(window, gc, cv.w, start, &cv.px[start * stride..row * stride])?;
        }
        shown.clone_from(&cv.px);
        *self.shown_of(win) = shown;
        self.c.flush()
    }

    fn dialog_needs_title_bar(&self) -> bool {
        false
    }

    fn set_own_title_bar(&mut self, own: bool) -> bool {
        self.own_title = own;
        for window in self.wins.clone().into_iter().filter(|&w| w != 0) {
            self.decorate(window);
        }
        for shown in &mut self.shown {
            shown.clear();
        }
        own
    }

    fn open_window(&mut self) -> io::Result<u8> {
        let n = self.wins.iter().skip(1).position(|&w| w == 0).map_or(self.wins.len(), |i| i + 1);
        if n > u8::MAX as usize {
            return Err(io::Error::other("too many windows"));
        }
        let (w, h) = self.sizes.first().copied().unwrap_or((800, 600));
        let atoms = self.atoms();
        let title = self.title.clone();
        let window = make_window(&mut self.c, &atoms, &title, w as i32, h as i32);
        self.decorate(window);
        if self.wins.len() <= n {
            self.wins.resize(n + 1, 0);
        }
        self.wins[n] = window;
        if self.sizes.len() <= n {
            self.sizes.resize(n + 1, (0, 0));
        }
        self.sizes[n] = (w, h);
        self.shown_of(Win::Main(n as u8)).clear();
        self.c.req(8, 0, &[W(window)]); // MapWindow
        let previous = std::mem::replace(&mut self.selected, n);
        self.wm_message(self.net_active_window, [2, 0, 0, 0, 0]);
        self.selected = previous;
        self.c.flush()?;
        self.pending.push_back(Event::Resize(Win::Main(n as u8), w.max(1), h.max(1)));
        Ok(n as u8)
    }

    fn close_window(&mut self, n: u8) {
        let n = n as usize;
        if n == 0 || n >= self.wins.len() || self.wins[n] == 0 {
            return;
        }
        let window = std::mem::replace(&mut self.wins[n], 0);
        self.shown_of(Win::Main(n as u8)).clear();
        self.c.req(4, 0, &[W(window)]); // DestroyWindow
        let _ = self.c.flush();
    }

    fn select(&mut self, n: u8) {
        self.selected = n as usize;
    }

    fn start_move(&mut self) {
        self.start_resize(8);
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        if std::mem::replace(&mut self.cursor, cursor) == cursor {
            return;
        }
        // Glyphs of the X "cursor" font; each is followed by its mask.
        let glyph = match cursor {
            Cursor::Arrow => None,
            Cursor::Text => Some(152), // xterm
            // top-left corner, top side, top-right, right, bottom-right, bottom, bottom-left, left
            Cursor::Resize(edge) => Some([134u16, 138, 136, 96, 14, 16, 12, 70][edge as usize % 8]),
        };
        let id = match (glyph, self.cursors.iter().find(|(c, _)| *c == cursor)) {
            (None, _) => 0, // the desktop's own arrow
            (_, Some(&(_, id))) => id,
            (Some(glyph), None) => {
                if self.cursor_font == 0 {
                    self.cursor_font = self.c.new_id();
                    self.c.req(45, 0, &[W(self.cursor_font), H(6), H(0), S(b"cursor")]); // OpenFont
                }
                let id = self.c.new_id();
                let font = self.cursor_font;
                // CreateGlyphCursor: black with a white outline.
                self.c.req(94, 0, &[W(id), W(font), W(font), H(glyph), H(glyph + 1), H(0), H(0), H(0), H(0xffff), H(0xffff), H(0xffff)]);
                self.cursors.push((cursor, id));
                id
            }
        };
        let window = self.cur();
        self.c.req(2, 0, &[W(window), W(0x4000), W(id)]); // ChangeWindowAttributes: cursor
        let _ = self.c.flush();
    }

    fn start_resize(&mut self, edge: u32) {
        self.c.req(27, 0, &[W(0)]); // UngrabPointer, so the WM can grab it
        let (x, y) = self.pointer_root;
        self.wm_message(self.net_wm_moveresize, [x as u32, y as u32, edge, 1, 1]);
    }

    fn hide_to_tray(&mut self) -> bool {
        self.close_dialog();
        self.c.req(10, 0, &[W(self.win)]); // UnmapWindow
        let _ = self.c.flush();
        true
    }

    fn show(&mut self) {
        self.c.req(8, 0, &[W(self.win)]); // MapWindow
        // Ask the window manager to bring it forward and give it the
        // keyboard: source 2 is a pager, which managers obey.
        let previous = std::mem::replace(&mut self.selected, 0);
        self.wm_message(self.net_active_window, [2, 0, 0, 0, 0]);
        self.selected = previous;
        let _ = self.c.flush();
        self.pending.push_back(Event::Redraw(FIRST));
    }

    fn minimize(&mut self) {
        self.wm_message(self.wm_change_state, [3, 0, 0, 0, 0]); // IconicState
    }

    fn toggle_maximize(&mut self) {
        let [vert, horz] = self.net_wm_maximized;
        self.wm_message(self.net_wm_state, [2, vert, horz, 1, 0]); // 2 = toggle
    }

    fn copy(&mut self, text: &str, file: bool) -> bool {
        self.owns_clipboard = own_clipboard(&mut self.c, self.win, self.clip.clipboard).unwrap_or(false);
        self.copied = text.to_string();
        self.copied_file = file;
        self.owns_clipboard
    }

    fn request_paste(&mut self) {
        // ConvertSelection: the answer arrives as SelectionNotify.
        let body = [W(self.win), W(self.clip.clipboard), W(self.clip.utf8_string), W(self.paste_prop), W(0)];
        self.c.req(24, 0, &body);
    }

    /// If our copy is still the clipboard content, pass it to a small
    /// background process so it survives the window closing.
    fn keep_clipboard(&mut self) {
        if !self.owns_clipboard || self.copied.is_empty() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let child = std::process::Command::new(exe)
            .args(["--hold-clipboard", if self.copied_file { "file" } else { "text" }])
            .env("FF_GUI_BACKEND", "x11")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = io::Write::write_all(&mut stdin, self.copied.as_bytes());
            }
        }
    }

    fn open_dialog(&mut self, w: i32, h: i32, title: &str) -> io::Result<()> {
        self.close_dialog();
        let owner = self.cur();
        let win = self.c.new_id();
        // Events: KeyPress | ButtonPress | PointerMotion | Exposure.
        let events = 0x1 | 0x4 | 0x40 | 0x8000;
        let c = &mut self.c;
        c.req(1, 0, &[W(win), W(c.root), H(0), H(0), H(w as u16), H(h as u16), H(0), H(1), W(0), W(0x800), W(events)]);
        let window_type = c.intern_atom("_NET_WM_WINDOW_TYPE")?;
        let type_dialog = c.intern_atom("_NET_WM_WINDOW_TYPE_DIALOG")?;
        let state_modal = c.intern_atom("_NET_WM_STATE_MODAL")?;
        c.set_property(win, ATOM_WM_NAME, ATOM_STRING, 8, title.as_bytes());
        c.set_property(win, self.net_wm_name, self.clip.utf8_string, 8, title.as_bytes());
        c.set_property(win, ATOM_WM_CLASS, ATOM_STRING, 8, b"ff-gui\0ff-gui\0");
        c.set_property(win, self.wm_protocols, ATOM_ATOM, 32, &words(&[self.wm_delete]));
        c.set_property(win, 68, 33, 32, &words(&[owner])); // WM_TRANSIENT_FOR: belongs to the search window
        c.set_property(win, window_type, ATOM_ATOM, 32, &words(&[type_dialog]));
        c.set_property(win, self.net_wm_state, ATOM_ATOM, 32, &words(&[state_modal]));
        // WM_NORMAL_HINTS: minimum size = maximum size, so it cannot be resized.
        let mut hints = [0u32; 18];
        hints[0] = 0x10 | 0x20;
        (hints[5], hints[6], hints[7], hints[8]) = (w as u32, h as u32, w as u32, h as u32);
        c.set_property(win, 40, 41, 32, &words(&hints));
        c.req(8, 0, &[W(win)]); // MapWindow
        self.dialog = win;
        self.dialog_shown.clear();
        Ok(())
    }

    fn close_dialog(&mut self) {
        if self.dialog != 0 {
            self.c.req(4, 0, &[W(self.dialog)]); // DestroyWindow
            self.dialog = 0;
            self.dialog_shown.clear();
        }
    }

    fn start_drag(&mut self, paths: &str, image: &Canvas, copy: bool) -> io::Result<()> {
        self.dropped = paths.to_string();
        // Every format we offer, for targets that read the full list.
        let a = self.clip;
        let types = words(&[a.uri_list, a.text_plain_utf8, a.text_plain, a.utf8_string, ATOM_STRING]);
        self.c.set_property(self.win, self.dnd.type_list, ATOM_ATOM, 32, &types);
        // The data is offered through this selection.
        self.c.req(22, 0, &[W(self.win), W(self.dnd.selection), W(0)]);
        debug::log(format!("drag started (our window {:#x})", self.win));
        let proxies = self.drop_proxies()?;

        // The image that follows the pointer: a window of its own,
        // unmanaged (override-redirect) so it can sit anywhere; we only
        // need to know when to repaint it.
        let ghost = self.c.new_id();
        let c = &mut self.c;
        let (w, h) = (image.w as u16, image.h as u16);
        c.req(1, 0, &[W(ghost), W(c.root), H(0), H(0), H(w), H(h), H(0), H(1), W(0), W(0x200 | 0x800), W(1), W(0x8000)]);
        let window_type = c.intern_atom("_NET_WM_WINDOW_TYPE")?;
        let type_dnd = c.intern_atom("_NET_WM_WINDOW_TYPE_DND")?;
        c.set_property(ghost, window_type, ATOM_ATOM, 32, &type_dnd.to_le_bytes());
        let u = (self.scale.round() as i32).max(1);
        let (px, py) = self.pointer_root;
        c.req(12, 0, &[W(ghost), H(1 | 2), H(0), W((px + 14 * u) as u32), W((py + 18 * u) as u32)]);
        c.req(8, 0, &[W(ghost)]); // MapWindow
        c.put_image(ghost, self.gc, image.w, 0, &image.px)?;
        self.drag = Some(Drag { copy, target: None, ghost, ghost_px: image.px.clone(), ghost_w: image.w, proxies, motions: 0 });
        Ok(())
    }

    fn drag_wants_copy(&mut self, copy: bool) {
        if let Some(drag) = self.drag.as_mut() {
            drag.copy = copy;
        }
    }
}
