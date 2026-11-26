//! A minimal X11 client: just the wire protocol over the local Unix socket,
//! with the handful of requests the window needs. No Xlib, no libc. The
//! window content is drawn client-side and sent as images.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// Predefined atoms.
pub const ATOM_ATOM: u32 = 4;
pub const ATOM_STRING: u32 = 31;
pub const ATOM_WM_NAME: u32 = 39;
pub const ATOM_WM_CLASS: u32 = 67;

// Event codes.
pub const EV_KEY_PRESS: u8 = 2;
pub const EV_BUTTON_PRESS: u8 = 4;
pub const EV_BUTTON_RELEASE: u8 = 5;
pub const EV_MOTION: u8 = 6;
pub const EV_EXPOSE: u8 = 12;
pub const EV_CONFIGURE: u8 = 22;
pub const EV_SELECTION_CLEAR: u8 = 29;
pub const EV_SELECTION_REQUEST: u8 = 30;
pub const EV_SELECTION_NOTIFY: u8 = 31;
pub const EV_CLIENT_MESSAGE: u8 = 33;
pub const EV_MAPPING: u8 = 34;

pub use crate::keys::*;

// Modifier bits in an event's state field.
pub const MOD_SHIFT: u16 = 1;
pub const MOD_LOCK: u16 = 2;
pub const MOD_CTRL: u16 = 4;
pub const MOD_ALT: u16 = 8;
const MOD_NUM: u16 = 0x10;
const MOD_LEVEL3: u16 = 0x80;

/// One field of a request body.
pub enum P<'a> {
    B(u8),
    H(u16),
    W(u32),
    S(&'a [u8]),
}
use P::*;

pub fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

pub fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

pub struct Conn {
    sock: UnixStream,
    out: Vec<u8>,
    inb: Vec<u8>,
    seq: u16,
    /// Events that arrived while waiting for a reply.
    queue: VecDeque<Vec<u8>>,
    id_base: u32,
    id_next: u32,
    pub root: u32,
    pub depth: u8,
    /// Largest request the server accepts, in bytes.
    max_request: usize,
    min_keycode: u8,
    max_keycode: u8,
    keysyms: Vec<u32>,
    keysyms_per_code: usize,
}

impl Conn {
    pub fn connect() -> io::Result<Conn> {
        let display = std::env::var("DISPLAY").map_err(|_| err("DISPLAY is not set"))?;
        // "[host]:N[.screen]" -> N; only local displays are supported.
        let num = display.rsplit(':').next().unwrap_or("").split('.').next().unwrap_or("");
        if num.is_empty() || num.parse::<u32>().is_err() {
            return Err(err(format!("cannot parse DISPLAY={display}")));
        }
        let mut sock = UnixStream::connect(format!("/tmp/.X11-unix/X{num}"))?;

        let (name, cookie) = match find_cookie(num) {
            Some(c) => (&b"MIT-MAGIC-COOKIE-1"[..], c),
            None => (&b""[..], Vec::new()),
        };
        let mut hello = vec![b'l', 0, 11, 0, 0, 0];
        hello.extend((name.len() as u16).to_le_bytes());
        hello.extend((cookie.len() as u16).to_le_bytes());
        hello.extend([0, 0]);
        for part in [name, &cookie[..]] {
            hello.extend(part);
            hello.resize(hello.len().next_multiple_of(4), 0);
        }
        sock.write_all(&hello)?;

        let mut head = [0u8; 8];
        sock.read_exact(&mut head)?;
        let mut b = vec![0u8; u16at(&head, 6) as usize * 4];
        sock.read_exact(&mut b)?;
        if head[0] != 1 {
            let reason = String::from_utf8_lossy(&b[..(head[1] as usize).min(b.len())]).into_owned();
            return Err(err(format!("X server refused the connection: {reason}")));
        }
        let vendor_len = (u16at(&b, 16) as usize).next_multiple_of(4);
        let screen = 32 + vendor_len + b[21] as usize * 8;
        if b[20] == 0 || b.len() < screen + 40 {
            return Err(err("X server reported no screen"));
        }
        // We send frames as little-endian 32-bit pixels.
        let depth = b[screen + 38];
        let mut formats = b[32 + vendor_len..screen].chunks_exact(8);
        if b[22] != 0 || !formats.any(|f| f[0] == depth && f[1] == 32) {
            return Err(err("unsupported display: need a little-endian 32-bit pixel format"));
        }
        let mut conn = Conn {
            sock,
            out: Vec::new(),
            inb: Vec::new(),
            seq: 0,
            queue: VecDeque::new(),
            id_base: u32at(&b, 4),
            id_next: 0,
            root: u32at(&b, screen),
            depth,
            max_request: (u16at(&b, 18) as usize * 4).max(4096),
            min_keycode: b[26],
            max_keycode: b[27],
            keysyms: Vec::new(),
            keysyms_per_code: 1,
        };
        conn.load_keymap()?;
        Ok(conn)
    }

    pub fn new_id(&mut self) -> u32 {
        self.id_next += 1;
        self.id_base | self.id_next
    }

    /// Queues a request and returns its sequence number.
    pub fn req(&mut self, opcode: u8, data: u8, body: &[P]) -> u16 {
        let start = self.out.len();
        self.out.extend([opcode, data, 0, 0]);
        for p in body {
            match *p {
                B(v) => self.out.push(v),
                H(v) => self.out.extend(v.to_le_bytes()),
                W(v) => self.out.extend(v.to_le_bytes()),
                S(v) => self.out.extend(v),
            }
        }
        let len = (self.out.len() - start).next_multiple_of(4);
        self.out.resize(start + len, 0);
        self.out[start + 2..start + 4].copy_from_slice(&((len / 4) as u16).to_le_bytes());
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.sock.write_all(&self.out)?;
        self.out.clear();
        Ok(())
    }

    /// Next complete message from the server. `wait` is how long to wait
    /// for one: forever (`None`), not at all (zero), or up to a timeout;
    /// in the last two cases `None` means nothing arrived.
    fn read_msg(&mut self, wait: Option<Duration>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if self.inb.len() >= 32 {
                // Replies and generic events carry extra 4-byte units.
                let extra = match self.inb[0] & 0x7f {
                    1 | 35 => u32at(&self.inb, 4) as usize * 4,
                    _ => 0,
                };
                if self.inb.len() >= 32 + extra {
                    return Ok(Some(self.inb.drain(..32 + extra).collect()));
                }
            }
            let mut chunk = [0u8; 16384];
            let n = match wait {
                None => self.sock.read(&mut chunk),
                Some(Duration::ZERO) => {
                    self.sock.set_nonblocking(true)?;
                    let r = self.sock.read(&mut chunk);
                    self.sock.set_nonblocking(false)?;
                    r
                }
                Some(timeout) => {
                    self.sock.set_read_timeout(Some(timeout))?;
                    let r = self.sock.read(&mut chunk);
                    self.sock.set_read_timeout(None)?;
                    r
                }
            };
            match n {
                Ok(0) => return Err(err("X server closed the connection")),
                Ok(n) => self.inb.extend(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Waits for the reply to request `seq`; `None` if the server answered
    /// with an error. Events seen meanwhile are kept for `next_event`.
    pub fn reply(&mut self, seq: u16) -> io::Result<Option<Vec<u8>>> {
        self.flush()?;
        loop {
            let Some(m) = self.read_msg(None)? else { continue };
            match m[0] {
                0 | 1 if u16at(&m, 2) == seq => return Ok((m[0] == 1).then_some(m)),
                0 | 1 => {}
                _ => self.queue.push_back(m),
            }
        }
    }

    /// Next event; see `read_msg` for `wait`.
    pub fn next_event(&mut self, wait: Option<Duration>) -> io::Result<Option<Vec<u8>>> {
        if let Some(m) = self.queue.pop_front() {
            return Ok(Some(m));
        }
        self.flush()?;
        loop {
            match self.read_msg(wait)? {
                Some(m) if m[0] > 1 => return Ok(Some(m)),
                Some(_) => {} // stray error or reply
                None => return Ok(None),
            }
        }
    }

    pub fn intern_atom(&mut self, name: &str) -> io::Result<u32> {
        let seq = self.req(16, 0, &[H(name.len() as u16), H(0), S(name.as_bytes())]);
        let r = self.reply(seq)?.ok_or_else(|| err("InternAtom failed"))?;
        Ok(u32at(&r, 8))
    }

    /// The child windows of `window`, bottom-most first.
    pub fn children(&mut self, window: u32) -> io::Result<Vec<u32>> {
        let seq = self.req(15, 0, &[W(window)]); // QueryTree
        let Some(r) = self.reply(seq)? else { return Ok(Vec::new()) };
        let n = u16at(&r, 16) as usize;
        Ok(r[32..].chunks_exact(4).take(n).map(|c| u32at(c, 0)).collect())
    }

    /// (is input-only, is viewable, bypasses the window manager) for `window`.
    pub fn window_kind(&mut self, window: u32) -> io::Result<Option<(bool, bool, bool)>> {
        let seq = self.req(3, 0, &[W(window)]); // GetWindowAttributes
        Ok(self.reply(seq)?.filter(|r| r.len() >= 28).map(|r| (u16at(&r, 12) == 2, r[26] == 2, r[27] != 0)))
    }

    /// The name of an atom (for diagnostics).
    pub fn atom_name(&mut self, atom: u32) -> String {
        let seq = self.req(17, 0, &[W(atom)]);
        match self.reply(seq) {
            Ok(Some(r)) => String::from_utf8_lossy(r.get(32..32 + u16at(&r, 8) as usize).unwrap_or(&[])).into_owned(),
            _ => format!("atom {atom}"),
        }
    }

    /// Replaces a property. `format` is 8 or 32 (bits per item).
    pub fn set_property(&mut self, window: u32, property: u32, ty: u32, format: u8, data: &[u8]) {
        let items = data.len() as u32 / (format as u32 / 8);
        self.req(18, 0, &[W(window), W(property), W(ty), B(format), S(&[0; 3]), W(items), S(data)]);
    }

    /// Reads a property as raw bytes, optionally deleting it.
    pub fn get_property(&mut self, window: u32, property: u32, delete: bool) -> io::Result<Vec<u8>> {
        let seq = self.req(20, delete as u8, &[W(window), W(property), W(0), W(0), W(1 << 16)]);
        let Some(r) = self.reply(seq)? else { return Ok(Vec::new()) };
        let len = u32at(&r, 16) as usize * (r[1] as usize / 8);
        Ok(r.get(32..32 + len).unwrap_or(&[]).to_vec())
    }

    /// Draws rows of 32-bit BGRX pixels (`width` wide) at row `y`, split
    /// into as many requests as the server's size limit needs.
    pub fn put_image(&mut self, window: u32, gc: u32, width: usize, y: usize, data: &[u8]) -> io::Result<()> {
        let stride = width * 4;
        let rows_per_req = ((self.max_request - 24) / stride).max(1);
        for (i, chunk) in data.chunks(rows_per_req * stride).enumerate() {
            let rows = chunk.len() / stride;
            let body = [
                W(window),
                W(gc),
                H(width as u16),
                H(rows as u16),
                H(0),
                H((y + i * rows_per_req) as u16),
                B(0),
                B(self.depth),
                H(0),
                S(chunk),
            ];
            self.req(72, 2, &body); // PutImage, ZPixmap
            if self.out.len() > 1 << 20 {
                self.flush()?;
            }
        }
        Ok(())
    }

    pub fn load_keymap(&mut self) -> io::Result<()> {
        let count = self.max_keycode - self.min_keycode + 1;
        let seq = self.req(101, 0, &[B(self.min_keycode), B(count), H(0)]);
        let r = self.reply(seq)?.ok_or_else(|| err("GetKeyboardMapping failed"))?;
        self.keysyms_per_code = (r[1] as usize).max(1);
        self.keysyms = r[32..].chunks_exact(4).map(|c| u32at(c, 0)).collect();
        Ok(())
    }

    /// Translates a key press into a character or a named key.
    pub fn key(&self, keycode: u8, state: u16) -> Key {
        if keycode < self.min_keycode {
            return Key::None;
        }
        let per = self.keysyms_per_code;
        let base = (keycode - self.min_keycode) as usize * per;
        let col = |i: usize| if i < per { self.keysyms.get(base + i).copied().unwrap_or(0) } else { 0 };

        // Keypad digits and operators sit in the shifted column under NumLock.
        if state & MOD_NUM != 0 && (0xffaa..=0xffb9).contains(&col(1)) {
            return Key::Char(keysym_char(col(1)).unwrap_or('?'));
        }
        // Core keymap columns: G1L1 G1L2 G2L1 G2L2 G1L3 G1L4 G2L3 G2L4.
        let shift = (state & MOD_SHIFT) as usize;
        let group = ((state >> 13) & 1) as usize * 2;
        let level3 = if state & MOD_LEVEL3 != 0 { 4 + group + shift } else { group + shift };
        for i in [level3, group + shift, group, shift, 0] {
            let sym = col(i);
            if sym == 0 {
                continue;
            }
            let Some(mut ch) = keysym_char(sym) else { return Key::Sym(sym) };
            // Shift fell back to an unshifted column, or Caps Lock is on.
            let fallback_shift = shift == 1 && i % 2 == 0;
            if (fallback_shift || state & MOD_LOCK != 0) && ch.is_lowercase() {
                ch = ch.to_uppercase().next().unwrap_or(ch);
            }
            return Key::Char(ch);
        }
        Key::None
    }
}

fn keysym_char(sym: u32) -> Option<char> {
    let cp = match sym {
        0x20..=0x7e | 0xa0..=0xff => sym,
        0x0100_0100..=0x0110_ffff => sym - 0x0100_0000, // direct Unicode
        0xffb0..=0xffb9 => sym - 0xffb0 + '0' as u32,   // keypad digits
        0xffaa..=0xffaf => b"*+,-./"[(sym - 0xffaa) as usize] as u32,
        0xff80 => ' ' as u32,
        0x20ac => 0x20ac,
        // Legacy Arabic key syms.
        0x5ac => 0x60c,
        0x5bb => 0x61b,
        0x5bf => 0x61f,
        0x5c1..=0x5da => sym - 0x5c1 + 0x621,
        0x5e0..=0x5f2 => sym - 0x5e0 + 0x640,
        _ => return None,
    };
    char::from_u32(cp)
}

/// Finds the MIT cookie for display `num` in the Xauthority file.
fn find_cookie(num: &str) -> Option<Vec<u8>> {
    let path = std::env::var_os("XAUTHORITY").map(std::path::PathBuf::from).or_else(|| {
        Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".Xauthority"))
    })?;
    let data = std::fs::read(path).ok()?;
    let mut rest = &data[..];
    // Entry: family u16, then address, number, name, data; each a big-endian
    // u16 length followed by that many bytes.
    while rest.len() >= 2 {
        rest = &rest[2..];
        let mut fields = [&[][..]; 4];
        for f in &mut fields {
            let len = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]) as usize;
            *f = rest.get(2..2 + len)?;
            rest = &rest[2 + len..];
        }
        let [_, number, name, cookie] = fields;
        if name == b"MIT-MAGIC-COOKIE-1" && (number.is_empty() || number == num.as_bytes()) {
            return Some(cookie.to_vec());
        }
    }
    None
}
