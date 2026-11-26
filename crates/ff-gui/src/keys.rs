//! Keys as the application sees them: a character, or an X keysym for
//! keys without one. Every backend translates its input into these.

// Key syms the UI cares about.
pub const K_BACKSPACE: u32 = 0xff08;
pub const K_TAB: u32 = 0xff09;
pub const K_BACK_TAB: u32 = 0xfe20;
pub const K_RETURN: u32 = 0xff0d;
pub const K_ESCAPE: u32 = 0xff1b;
pub const K_HOME: u32 = 0xff50;
pub const K_LEFT: u32 = 0xff51;
pub const K_RIGHT: u32 = 0xff53;
pub const K_DELETE: u32 = 0xffff;
pub const K_UP: u32 = 0xff52;
pub const K_DOWN: u32 = 0xff54;
pub const K_PAGE_UP: u32 = 0xff55;
pub const K_PAGE_DOWN: u32 = 0xff56;
pub const K_END: u32 = 0xff57;
pub const K_MENU: u32 = 0xff67;
pub const K_KP_ENTER: u32 = 0xff8d;
pub const K_F1: u32 = 0xffbe;
pub const K_F2: u32 = 0xffbf;
pub const K_F3: u32 = 0xffc0;
pub const K_F4: u32 = 0xffc1;
pub const K_F5: u32 = 0xffc2;
pub const K_F6: u32 = 0xffc3;
pub const K_F7: u32 = 0xffc4;

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(windows, allow(dead_code))] // None: a key X11 cannot name
pub enum Key {
    Char(char),
    Sym(u32),
    None,
}
