//! Client-side drawing: a pixel buffer, anti-aliased text and file icons.

use std::collections::HashMap;
#[cfg(not(windows))]
use std::io;

/// The window content as 32-bit BGRX pixels, ready to send to the X server.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
    /// Horizontal clip range (x0, x1) applied to all drawing.
    pub clip: (i32, i32),
}

static THEME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Switches all drawing to theme `THEMES[index]`.
pub fn set_theme(index: usize) {
    THEME.store(index.min(THEMES.len() - 1), std::sync::atomic::Ordering::Relaxed);
}

/// True if the current theme is dark.
pub fn is_dark() -> bool {
    theme().dark
}

/// The theme drawing uses now.
fn theme() -> &'static Theme {
    &THEMES[THEME.load(std::sync::atomic::Ordering::Relaxed)]
}

/// White that stays white in every theme: text on a coloured fill.
pub const ON_ACCENT: u32 = 0xff_fffe;

/// The accent colour: the desktop's (Windows) or a blue, drawn a shade
/// lighter in dark themes. See [`set_accent`].
pub const ACCENT: u32 = 0xff_fffd;

/// A red of the same lightness and strength as [`ACCENT`]: for warnings
/// that sit beside accent-coloured things (a nearly full drive).
pub const ACCENT_RED: u32 = 0xff_fffc;

static ACCENT_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x00_67c0);

/// Sets the colour [`ACCENT`] stands for.
pub fn set_accent(rgb: u32) {
    ACCENT_RGB.store(rgb & 0xff_ffff, std::sync::atomic::Ordering::Relaxed);
}

/// The surfaces and inks every theme defines. Drawing code uses these
/// values (those of the Light theme); [`tone`] turns them into the
/// current theme's. They must differ from any other colour drawn.
pub mod palette {
    /// The window behind everything (Windows' "Mica").
    pub const BASE: u32 = 0xf3f3f3;
    /// The results list, a layer above the base.
    pub const CARD: u32 = 0xfbfbfb;
    /// Text boxes, menus and dialogs.
    pub const FIELD: u32 = 0xffffff;
    /// Hairlines and borders.
    pub const STROKE: u32 = 0xe5e5e5;
    /// A row or item under the pointer.
    pub const HOVER: u32 = 0xf0f0f0;
    /// A selected row.
    pub const PICKED: u32 = 0xe9e9e9;
    pub const INK: u32 = 0x1b1b1b;
    pub const DIM_INK: u32 = 0x616161;
    /// Scrollbar thumbs and other quiet controls.
    pub const QUIET: u32 = 0x8a8a8a;

    /// In the order of [`super::Theme::colors`].
    pub(super) const ALL: [u32; 9] = [BASE, CARD, FIELD, STROKE, HOVER, PICKED, INK, DIM_INK, QUIET];
}

/// A colour theme: its [`palette`] colours, and whether it is dark (other
/// greys are then flipped and the accent lightened).
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    /// Base, card, field, stroke, hover, picked, ink, dim ink, quiet.
    colors: [u32; 9],
}

/// Accent colours to choose instead of the desktop's (Windows 11's own),
/// as (menu label, 0xRRGGBB). The first is also the default where the
/// desktop has none.
pub const ACCENTS: [(&str, u32); 7] = [
    ("Blue accent", 0x0067c0),
    ("Teal accent", 0x038387),
    ("Green accent", 0x107c10),
    ("Orange accent", 0xca5010),
    ("Red accent", 0xc42b1c),
    ("Purple accent", 0x744da9),
    ("Grey accent", 0x5d5a58),
];

/// Every theme. Adding one is one entry here.
pub const THEMES: [Theme; 6] = [
    Theme { name: "Light", dark: false, colors: palette::ALL },
    Theme {
        name: "Dark",
        dark: true,
        colors: [0x202020, 0x272727, 0x2d2d2d, 0x3a3a3a, 0x323232, 0x383838, 0xf3f3f3, 0xababab, 0x9a9a9a],
    },
    Theme {
        name: "Graphite (dark grey)",
        dark: true,
        colors: [0x2b2b2b, 0x333333, 0x3a3a3a, 0x494949, 0x3e3e3e, 0x464646, 0xeeeeee, 0xb2b2b2, 0xa2a2a2],
    },
    Theme {
        name: "Mist (light grey)",
        dark: false,
        colors: [0xe3e5e8, 0xeef0f2, 0xf8f9fa, 0xd2d5da, 0xe1e4e8, 0xd7dbe0, 0x1d2126, 0x585e66, 0x878d95],
    },
    Theme {
        name: "Dusk (blue-grey dark)",
        dark: true,
        colors: [0x1d2330, 0x242b39, 0x2b3343, 0x3a4458, 0x30394b, 0x39445a, 0xe5e9f1, 0xa2acbe, 0x8d98ac],
    },
    Theme {
        name: "Sand (warm light)",
        dark: false,
        colors: [0xf1eadd, 0xf9f5ec, 0xfffcf5, 0xe2d7c3, 0xede4d4, 0xe6d9c2, 0x2b2419, 0x6a5e4b, 0x9a8d78],
    },
];

/// The colour to really draw for a colour of the Light theme. The
/// [`palette`] colours take the current theme's values. In a dark theme
/// other greys are flipped (white paper becomes near-black, dark ink
/// becomes light), pale tints become deep ones of the same hue, and
/// strong colours stay as they are.
pub fn tone(color: u32) -> u32 {
    let theme = theme();
    let dark = theme.dark;
    if color == ON_ACCENT {
        return 0xff_ffff;
    }
    if color == ACCENT || color == ACCENT_RED {
        let rgb = ACCENT_RGB.load(std::sync::atomic::Ordering::Relaxed);
        let rgb = if color == ACCENT_RED { as_red(rgb) } else { rgb };
        if !dark {
            return rgb;
        }
        // Windows uses a lighter accent on dark: mix 45% white in.
        let [b, g, r] = bgr(rgb).map(|c| c as u32);
        let light = |c: u32| c + (255 - c) * 45 / 100;
        return light(r) << 16 | light(g) << 8 | light(b);
    }
    if let Some(i) = palette::ALL.iter().position(|&c| c == color) {
        return theme.colors[i];
    }
    if !dark {
        return color;
    }
    let [b, g, r] = bgr(color).map(|c| c as u32);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max - min < 24 {
        // A grey: mirror its brightness into the range 0x1c..0xec.
        let flipped = 0x1c + (255 - (r + g + b) / 3) * (0xec - 0x1c) / 255;
        flipped << 16 | flipped << 8 | flipped
    } else if min > 0xc0 {
        // A pale tint: keep what makes it a tint, on a dark base.
        let deep = |c: u32| (0x26 + (c - min) * 3 / 2).min(0xff);
        deep(r) << 16 | deep(g) << 8 | deep(b)
    } else {
        color
    }
}

fn bgr(color: u32) -> [u8; 3] {
    [color as u8, (color >> 8) as u8, (color >> 16) as u8]
}

impl Canvas {
    pub fn new() -> Canvas {
        Canvas { w: 0, h: 0, px: Vec::new(), clip: (0, i32::MAX) }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        (self.w, self.h) = (w, h);
        self.px.clear();
        self.px.resize(w * h * 4, 0xff);
    }

    /// Fills a rectangle (clipped to the canvas) with an 0xRRGGBB color.
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let clamp = |v: i32, max: usize| v.clamp(0, max as i32) as usize;
        let (x0, x1) = (clamp(x.max(self.clip.0), self.w), clamp(x.saturating_add(w).min(self.clip.1), self.w));
        let x1 = x1.max(x0);
        let (y0, y1) = (clamp(y, self.h), clamp(y.saturating_add(h), self.h));
        let [b, g, r] = bgr(tone(color));
        for row in y0..y1 {
            let start = row * self.w * 4;
            for p in self.px[start + x0 * 4..start + x1 * 4].chunks_exact_mut(4) {
                p.copy_from_slice(&[b, g, r, 0]);
            }
        }
    }

    /// Fills a rectangle with corners rounded to radius `r`, anti-aliased.
    pub fn round(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        let r = r.min(w / 2).min(h / 2).max(0);
        if r == 0 {
            return self.fill(x, y, w, h, color);
        }
        self.fill(x, y + r, w, h - 2 * r, color);
        self.fill(x + r, y, w - 2 * r, r, color);
        self.fill(x + r, y + h - r, w - 2 * r, r, color);
        let c = bgr(tone(color));
        let rf = r as f32;
        // The four corners: coverage from 4x4 samples per pixel.
        for dy in 0..r {
            for dx in 0..r {
                let mut inside = 0;
                for s in 0..16 {
                    let px = dx as f32 + (s % 4) as f32 * 0.25 + 0.125;
                    let py = dy as f32 + (s / 4) as f32 * 0.25 + 0.125;
                    let (ex, ey) = (rf - px, rf - py);
                    inside += (ex <= 0.0 || ey <= 0.0 || ex * ex + ey * ey <= rf * rf) as u32;
                }
                let a = inside * 255 / 16;
                self.blend(x + dx, y + dy, c, a);
                self.blend(x + w - 1 - dx, y + dy, c, a);
                self.blend(x + dx, y + h - 1 - dy, c, a);
                self.blend(x + w - 1 - dx, y + h - 1 - dy, c, a);
            }
        }
    }

    /// A rounded outline `t` thick, drawn as a rounded fill of `color`
    /// with a rounded fill of `inside` on top.
    pub fn round_frame(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, t: i32, color: u32, inside: u32) {
        self.round(x, y, w, h, r, color);
        self.round(x + t, y + t, w - 2 * t, h - 2 * t, (r - t).max(0), inside);
    }

    /// A 1-`t`-thick rectangle outline.
    pub fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, color: u32) {
        self.fill(x, y, w, t, color);
        self.fill(x, y + h - t, w, t, color);
        self.fill(x, y, t, h, color);
        self.fill(x + w - t, y, t, h, color);
    }

    /// Mixes `color` (B, G, R) into one pixel with opacity `alpha` (0-255).
    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u32) {
        let clipped = x < self.clip.0 || x >= self.clip.1;
        if alpha == 0 || clipped || x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        for k in 0..3 {
            let dst = self.px[i + k] as u32;
            self.px[i + k] = ((dst * (255 - alpha) + color[k] as u32 * alpha + 127) / 255) as u8;
        }
    }
}

/// Windows draws text with GDI (`gdi_text.rs`); elsewhere text is drawn
/// from the font outlines here.
#[cfg(windows)]
pub use crate::gdi_text::Text;
#[cfg(not(windows))]
pub use ttf_text::Text;

#[cfg(not(windows))]
mod ttf_text {
use super::*;
use std::collections::HashSet;

struct Glyph {
    advance: f32,
    /// Offset of the bitmap from the pen position and the baseline.
    left: i32,
    top: i32,
    w: usize,
    coverage: Vec<u8>,
}

type Point = (f32, f32);

/// Collects a glyph outline as straight segments in pixel space (y down).
struct Outline {
    scale: f32,
    start: Point,
    at: Point,
    lines: Vec<(Point, Point)>,
}

impl Outline {
    fn px(&self, x: f32, y: f32) -> Point {
        (x * self.scale, -y * self.scale)
    }

    fn line(&mut self, to: Point) {
        self.lines.push((self.at, to));
        self.at = to;
    }

    /// Flattens a curve given as a function of t in 0..=1.
    fn curve(&mut self, reach: f32, point: impl Fn(f32) -> Point) {
        let steps = (reach / 3.0).ceil().clamp(2.0, 24.0) as usize;
        for i in 1..=steps {
            self.line(point(i as f32 / steps as f32));
        }
    }
}

fn dist(a: Point, b: Point) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.at = self.px(x, y);
        self.start = self.at;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.line(self.px(x, y));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (a, b, c) = (self.at, self.px(x1, y1), self.px(x, y));
        self.curve(dist(a, b) + dist(b, c), |t| {
            let (m, n, o) = ((1.0 - t) * (1.0 - t), 2.0 * t * (1.0 - t), t * t);
            (m * a.0 + n * b.0 + o * c.0, m * a.1 + n * b.1 + o * c.1)
        });
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (a, b, c, d) = (self.at, self.px(x1, y1), self.px(x2, y2), self.px(x, y));
        self.curve(dist(a, b) + dist(b, c) + dist(c, d), |t| {
            let s = 1.0 - t;
            let (m, n, o, p) = (s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t);
            (m * a.0 + n * b.0 + o * c.0 + p * d.0, m * a.1 + n * b.1 + o * c.1 + p * d.1)
        });
    }

    fn close(&mut self) {
        self.line(self.start);
    }
}

/// Exact-area coverage rasterizer: every edge adds its signed area to an
/// accumulation buffer; a running sum then yields per-pixel coverage.
fn rasterize(lines: &[(Point, Point)], w: usize, h: usize) -> Vec<u8> {
    let mut acc = vec![0.0f32; w * h + 2];
    let mut add = |i: usize, v: f32| {
        if let Some(cell) = acc.get_mut(i) {
            *cell += v;
        }
    };
    for &(a, b) in lines {
        if a.1 == b.1 {
            continue;
        }
        let (dir, p0, p1) = if a.1 < b.1 { (1.0, a, b) } else { (-1.0, b, a) };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        for y in p0.1.max(0.0) as usize..h.min(p1.1.ceil() as usize) {
            let row = y * w;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (x0, x1) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let x0floor = x0.floor();
            let x0i = x0floor.max(0.0) as usize;
            let x1ceil = x1.ceil();
            let x1i = x1ceil.max(0.0) as usize;
            if x1i <= x0i + 1 {
                let xmf = 0.5 * (x + xnext) - x0floor;
                add(row + x0i, d - d * xmf);
                add(row + x0i + 1, d * xmf);
            } else {
                let s = (x1 - x0).recip();
                let x0f = x0 - x0floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                add(row + x0i, d * a0);
                if x1i == x0i + 2 {
                    add(row + x0i + 1, d * (1.0 - a0 - am));
                } else {
                    let a1 = s * (1.5 - x0f);
                    add(row + x0i + 1, d * (a1 - a0));
                    for xi in x0i + 2..x1i - 1 {
                        add(row + xi, d * s);
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    add(row + x1i - 1, d * (1.0 - a2 - am));
                }
                add(row + x1i, d * am);
            }
            x = xnext;
        }
    }
    let mut sum = 0.0;
    acc[..w * h]
        .iter()
        .map(|v| {
            sum += v;
            (sum.abs().min(1.0) * 255.0) as u8
        })
        .collect()
}

type Face = ttf_parser::Face<'static>;

/// Anti-aliased text in one size, with glyphs rasterized once and cached.
pub struct Text {
    fonts: Vec<Face>,
    loaded: Vec<String>,
    /// Unicode blocks we already asked fontconfig about.
    asked: HashSet<u32>,
    px: f32,
    cache: HashMap<char, Glyph>,
    pub ascent: i32,
    pub height: i32,
}

fn fc_match(pattern: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let file = String::from_utf8(out.stdout).ok()?;
    (!file.is_empty()).then_some(file)
}


fn load_font(path: &str) -> Option<Face> {
    // Fonts stay loaded for the life of the program.
    let data: &'static [u8] = Box::leak(std::fs::read(path).ok()?.into_boxed_slice());
    let face = Face::parse(data, 0).ok()?;
    // Reject fonts we cannot actually draw.
    (make_glyph(&face, 'a', 16.0)?.w > 0).then_some(face)
}

fn make_glyph(face: &Face, ch: char, px: f32) -> Option<Glyph> {
    let id = face.glyph_index(ch)?;
    let scale = px / face.units_per_em() as f32;
    let advance = face.glyph_hor_advance(id).unwrap_or(0) as f32 * scale;
    let mut outline = Outline { scale, start: (0.0, 0.0), at: (0.0, 0.0), lines: Vec::new() };
    face.outline_glyph(id, &mut outline);
    if outline.lines.is_empty() {
        return Some(Glyph { advance, left: 0, top: 0, w: 0, coverage: Vec::new() }); // e.g. a space
    }
    // Shift the outline into a bitmap that just contains it.
    let points = || outline.lines.iter().flat_map(|&(a, b)| [a, b]);
    let left = points().map(|p| p.0).fold(f32::MAX, f32::min).floor();
    let top = points().map(|p| p.1).fold(f32::MAX, f32::min).floor();
    let w = (points().map(|p| p.0).fold(f32::MIN, f32::max) - left).ceil() as usize + 1;
    let h = (points().map(|p| p.1).fold(f32::MIN, f32::max) - top).ceil() as usize + 1;
    let moved: Vec<_> =
        outline.lines.iter().map(|&(a, b)| ((a.0 - left, a.1 - top), (b.0 - left, b.1 - top))).collect();
    Some(Glyph { advance, left: left as i32, top: top as i32, w, coverage: rasterize(&moved, w, h.min(4096)) })
}

impl Text {
    /// Loads the UI font: FF_GUI_FONT (a font file), else the system's
    /// default sans-serif, else a few well-known locations.
    pub fn load(px: f32) -> io::Result<Text> {
        let mut candidates: Vec<String> = std::env::var("FF_GUI_FONT").into_iter().collect();
        candidates.extend(fc_match("sans"));
        candidates.extend(
            [
                "/usr/share/fonts/truetype/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
            ]
            .map(String::from),
        );
        let (path, face) = candidates
            .into_iter()
            .find_map(|p| load_font(&p).map(|f| (p, f)))
            .ok_or_else(|| io::Error::other("no usable font found; set FF_GUI_FONT to a .ttf file"))?;
        let scale = px / face.units_per_em() as f32;
        let (ascent, descent) = (face.ascender() as f32 * scale, face.descender() as f32 * scale);
        Ok(Text {
            fonts: vec![face],
            loaded: vec![path],
            asked: HashSet::new(),
            px,
            cache: HashMap::new(),
            ascent: ascent.ceil() as i32,
            height: (ascent - descent).ceil() as i32,
        })
    }

    fn glyph(&mut self, ch: char) -> &Glyph {
        let ch = if ch.is_control() { '?' } else { ch };
        if !self.cache.contains_key(&ch) {
            let has = |f: &Face| f.glyph_index(ch).is_some();
            let mut font = self.fonts.iter().position(has);
            // Not in any loaded font: ask fontconfig once per Unicode block.
            if font.is_none() && self.asked.insert(ch as u32 >> 7) {
                if let Some(path) = fc_match(&format!(":charset={:x}", ch as u32)) {
                    if !self.loaded.contains(&path) {
                        if let Some(f) = load_font(&path) {
                            font = has(&f).then_some(self.fonts.len());
                            self.fonts.push(f);
                        }
                        self.loaded.push(path);
                    }
                }
            }
            // Unknown everywhere: the primary font's "missing" box.
            let glyph = match font {
                Some(i) => make_glyph(&self.fonts[i], ch, self.px),
                None => make_glyph(&self.fonts[0], '\u{25a1}', self.px).or_else(|| make_glyph(&self.fonts[0], '?', self.px)),
            };
            let blank = Glyph { advance: self.px / 2.0, left: 0, top: 0, w: 0, coverage: Vec::new() };
            self.cache.insert(ch, glyph.unwrap_or(blank));
        }
        &self.cache[&ch]
    }

    pub fn width(&mut self, s: &str) -> i32 {
        s.chars().map(|ch| self.glyph(ch).advance).sum::<f32>().ceil() as i32
    }

    /// Byte index in `s` of the character boundary nearest to offset `x`.
    pub fn index_at(&mut self, s: &str, x: i32) -> usize {
        let mut pen = 0.0;
        for (i, ch) in s.char_indices() {
            let advance = self.glyph(ch).advance;
            if (x as f32) < pen + advance / 2.0 {
                return i;
            }
            pen += advance;
        }
        s.len()
    }

    /// Draws `s` with its top-left at (`x`, `top`), shortened with an
    /// ellipsis if it is wider than `max_w`.
    pub fn draw(&mut self, cv: &mut Canvas, x: i32, top: i32, s: &str, max_w: i32, color: u32) {
        let too_wide = self.width(s) > max_w;
        let ellipsis = if too_wide { self.glyph('…').advance } else { 0.0 };
        let limit = (x + max_w) as f32;
        let baseline = top + self.ascent;
        let color = bgr(tone(color));
        let mut pen = x as f32;
        let mut chars = s.chars();
        loop {
            let mut next = chars.next();
            if let Some(ch) = next {
                if too_wide && pen + self.glyph(ch).advance + ellipsis > limit {
                    next = None;
                }
            }
            let last = next.is_none();
            if last && !too_wide {
                break;
            }
            let g = self.glyph(next.unwrap_or('…'));
            if pen + g.advance <= limit + 0.5 {
                let (gx, gy) = (pen.round() as i32 + g.left, baseline + g.top);
                for (i, &a) in g.coverage.iter().enumerate() {
                    cv.blend(gx + (i % g.w) as i32, gy + (i / g.w) as i32, color, a as u32);
                }
            }
            pen += g.advance;
            if last {
                break;
            }
        }
    }
}
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Folder,
    File,
    Text,
    Code,
    Document,
    Pdf,
    Image,
    Audio,
    Video,
    Archive,
    Program,
    Link,
    /// The application icon: an eye.
    App,
}

impl Icon {
    /// Picks an icon from the entry type and file extension.
    pub fn of(name: &[u8], is_dir: bool, is_symlink: bool) -> Icon {
        if is_dir {
            return Icon::Folder;
        }
        if is_symlink {
            return Icon::Link;
        }
        let ext = match name.iter().rposition(|&b| b == b'.') {
            Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
            _ => return Icon::File,
        };
        match &ext[..] {
            b"txt" | b"md" | b"log" | b"rst" | b"csv" | b"ini" | b"conf" | b"cfg" | b"toml" | b"yaml" | b"yml"
            | b"json" | b"xml" | b"desktop" => Icon::Text,
            b"rs" | b"c" | b"h" | b"cpp" | b"hpp" | b"cc" | b"py" | b"js" | b"ts" | b"jsx" | b"tsx" | b"go"
            | b"java" | b"kt" | b"swift" | b"rb" | b"php" | b"sh" | b"bash" | b"zsh" | b"lua" | b"pl" | b"cs"
            | b"html" | b"css" | b"sql" => Icon::Code,
            b"doc" | b"docx" | b"odt" | b"rtf" | b"xls" | b"xlsx" | b"ods" | b"ppt" | b"pptx" | b"odp" | b"epub" => {
                Icon::Document
            }
            b"pdf" => Icon::Pdf,
            b"png" | b"jpg" | b"jpeg" | b"gif" | b"bmp" | b"svg" | b"webp" | b"ico" | b"tif" | b"tiff" | b"heic"
            | b"avif" => Icon::Image,
            b"mp3" | b"wav" | b"flac" | b"ogg" | b"m4a" | b"aac" | b"opus" => Icon::Audio,
            b"mp4" | b"mkv" | b"avi" | b"mov" | b"webm" | b"wmv" | b"flv" | b"m4v" => Icon::Video,
            b"zip" | b"tar" | b"gz" | b"xz" | b"bz2" | b"zst" | b"7z" | b"rar" | b"tgz" | b"rpm" | b"deb" | b"iso"
            | b"jar" => Icon::Archive,
            b"exe" | b"appimage" | b"bin" | b"run" | b"so" => Icon::Program,
            _ => Icon::File,
        }
    }

    /// Color of the icon at a point of the unit square, if it covers it.
    fn sample(self, u: f32, v: f32) -> Option<u32> {
        let rounded = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
            let dx = (x0 + r - u).max(u - (x1 - r)).max(0.0);
            let dy = (y0 + r - v).max(v - (y1 - r)).max(0.0);
            u >= x0 && u <= x1 && v >= y0 && v <= y1 && dx * dx + dy * dy <= r * r
        };
        if self == Icon::App {
            return crate::app_icon::sample(u, v);
        }
        if self == Icon::Folder {
            return if rounded(0.05, 0.27, 0.95, 0.87, 0.07) {
                Some(if v < 0.37 { 0xe5ac2c } else { 0xf5c542 })
            } else if rounded(0.05, 0.14, 0.47, 0.42, 0.07) {
                Some(0xd3951c) // the tab, behind the body
            } else {
                None
            };
        }
        // A page with a folded top-right corner; `inset` shrinks it to
        // tell the outline from the inside.
        let page = |inset: f32| {
            u >= 0.18 + inset && u <= 0.82 - inset && v >= 0.06 + inset && v <= 0.94 - inset && u - v <= 0.52 - inset * 1.4
        };
        if !page(0.0) {
            return None;
        }
        const EDGE: u32 = 0x7b838e;
        if !page(0.045) {
            return Some(EDGE);
        }
        if u > 0.58 && v < 0.30 {
            return Some(if u < 0.625 || v > 0.255 { EDGE } else { 0xdde1e6 }); // the fold
        }
        let band = (0.52..=0.80).contains(&v);
        let color = match self {
            Icon::Text => {
                let on_line = [0.44, 0.58, 0.72].iter().any(|c| (v - c).abs() < 0.035);
                return Some(if on_line && (0.32..=0.68).contains(&u) { 0x9aa1aa } else { 0xffffff });
            }
            Icon::Code => 0x3b7dd8,
            Icon::Document => 0x2f9e8f,
            Icon::Pdf => 0xd93025,
            Icon::Image => 0x3fa66b,
            Icon::Audio => 0x9b59b6,
            Icon::Video => 0xe2662f,
            Icon::Archive => 0xb07d3a,
            Icon::Program => 0x4a4f57,
            Icon::Link => 0x9aa1aa,
            _ => return Some(0xffffff),
        };
        Some(if band { color } else { 0xffffff })
    }
}

impl Icon {
    /// Renders the icon as `size` x `size` BGRA pixels.
    pub fn render(self, size: usize) -> Vec<[u8; 4]> {
        crate::app_icon::render(size, |u, v| self.sample(u, v))
    }
}

/// File icons drawn from shapes and cached as images at the UI size.
pub struct Icons {
    pub size: usize,
    cache: HashMap<Icon, Vec<[u8; 4]>>,
    /// The system's icons, by file type (or drive); None where it has
    /// none: the Windows shell's, or the desktop's icon theme elsewhere.
    system: HashMap<String, Option<Vec<[u8; 4]>>>,
    /// Elsewhere: the desktop's icon theme, read on first use.
    #[cfg(not(windows))]
    themes: Option<crate::linux_icons::Themes>,
    /// Elsewhere: the text, background and accent colours the theme's
    /// icons were drawn in (KDE's monochrome icons take them).
    #[cfg(not(windows))]
    scheme: [u32; 3],
    /// Windows: icons of files that carry their own (programs, shortcuts),
    /// by path, read in the background; asked for but not in yet: absent.
    #[cfg(windows)]
    own: HashMap<std::path::PathBuf, Option<Vec<[u8; 4]>>>,
    #[cfg(windows)]
    asked: std::collections::HashSet<std::path::PathBuf>,
    /// Windows: type icons asked for and not in yet.
    #[cfg(windows)]
    asked_kinds: std::collections::HashSet<String>,
    #[cfg(windows)]
    loader: Option<crate::shell_icons::Loader>,
}

impl Icons {
    pub fn new(size: usize) -> Icons {
        Icons {
            size,
            cache: HashMap::new(),
            system: HashMap::new(),
            #[cfg(not(windows))]
            themes: None,
            #[cfg(not(windows))]
            scheme: [0; 3],
            #[cfg(windows)]
            own: HashMap::new(),
            #[cfg(windows)]
            asked: Default::default(),
            #[cfg(windows)]
            asked_kinds: Default::default(),
            #[cfg(windows)]
            loader: None,
        }
    }

    pub fn draw(&mut self, cv: &mut Canvas, x: i32, y: i32, icon: Icon) {
        let size = self.size;
        let image = self.cache.entry(icon).or_insert_with(|| icon.render(size));
        blit(cv, x, y, size, image);
    }

    /// Takes in the icons read since the last call; true if any came
    /// (the window should be drawn again).
    pub fn take_loaded(&mut self) -> bool {
        #[cfg(windows)]
        if let Some(loader) = &self.loader {
            let ready = loader.ready();
            let any = !ready.is_empty();
            for loaded in ready {
                match loaded {
                    crate::shell_icons::Loaded::Own(path, pixels) => drop(self.own.insert(path, pixels)),
                    crate::shell_icons::Loaded::Kind(key, pixels) => {
                        self.asked_kinds.remove(&key);
                        self.system.insert(key, pixels);
                    }
                }
            }
            return any;
        }
        false
    }

    /// True while icons are still being read.
    pub fn loading(&self) -> bool {
        #[cfg(windows)]
        return self.asked.len() > self.own.len() || !self.asked_kinds.is_empty();
        #[cfg(not(windows))]
        false
    }

    /// Draws the icon of index entry `path`: on Windows with `system`, the
    /// one Explorer shows (by file type; a drive's or a program's own),
    /// else the built-in one.
    #[cfg_attr(not(windows), allow(unused_variables))]
    #[allow(clippy::too_many_arguments)]
    pub fn draw_entry(
        &mut self,
        cv: &mut Canvas,
        x: i32,
        y: i32,
        name: &[u8],
        is_dir: bool,
        is_symlink: bool,
        path: &std::path::Path,
        drive: bool,
        system: bool,
    ) {
        #[cfg(windows)]
        if system {
            let size = self.size;
            // A program or shortcut: its own icon, once read; until then
            // the icon of its type.
            if !is_dir && !drive && crate::shell_icons::has_own_icon(name) {
                if let Some(Some(image)) = self.own.get(path) {
                    return blit(cv, x, y, size, image);
                }
                if self.asked.insert(path.to_path_buf()) {
                    let job = crate::shell_icons::Job::Own(path.to_path_buf(), size);
                    self.loader.get_or_insert_with(crate::shell_icons::Loader::new).request(job);
                }
            }
            // A stand-in name of the same type is all the shell needs.
            let (key, query) = if drive {
                (path.display().to_string(), String::new())
            } else if is_dir {
                ("<folder>".to_string(), "folder".to_string())
            } else {
                let ext = match name.iter().rposition(|&b| b == b'.') {
                    Some(i) if i > 0 => String::from_utf8_lossy(&name[i..]).to_ascii_lowercase(),
                    _ => String::new(),
                };
                (ext.clone(), format!("file{ext}"))
            };
            // Read on the loader's thread (the shell can take a while for a
            // type it has not shown yet); the built-in icon stands in until
            // then, a frame or two.
            match self.system.get(&key) {
                Some(Some(image)) => return blit(cv, x, y, size, image),
                Some(None) => {}
                None => {
                    if self.asked_kinds.insert(key.clone()) {
                        let real = drive.then(|| path.to_path_buf());
                        let job = crate::shell_icons::Job::Kind { key, query, is_dir, real, size };
                        self.loader.get_or_insert_with(crate::shell_icons::Loader::new).request(job);
                    }
                }
            }
        }
        // The desktop's icon theme (Linux and others).
        #[cfg(not(windows))]
        if system {
            let size = self.size;
            // Icons drawn for another theme are drawn again.
            let scheme = [tone(palette::INK), tone(palette::CARD), tone(ACCENT)];
            if self.scheme != scheme {
                self.scheme = scheme;
                self.system.clear();
                // A dark window takes the theme's dark variant.
                self.themes = None;
            }
            let themes = self.themes.get_or_insert_with(|| crate::linux_icons::Themes::load(is_dark()));
            if themes.usable() {
                let key = if drive {
                    "<drive>".to_string()
                } else if is_dir {
                    "<folder>".to_string()
                } else {
                    // The extension decides the type (and so the icon).
                    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
                    name.split_once('.').map_or(String::new(), |(_, ext)| ext.to_string())
                };
                let image = self.system.entry(key).or_insert_with(|| themes.icon(name, is_dir, drive, size, scheme));
                if let Some(image) = image {
                    return blit(cv, x, y, size, image);
                }
            }
        }
        self.draw(cv, x, y, Icon::of(name, is_dir, is_symlink));
    }
}

/// Draws `size` x `size` BGRA pixels at (`x`, `y`), blending by alpha.
fn blit(cv: &mut Canvas, x: i32, y: i32, size: usize, image: &[[u8; 4]]) {
    for (i, p) in image.iter().enumerate() {
        cv.blend(x + (i % size) as i32, y + (i / size) as i32, [p[0], p[1], p[2]], p[3] as u32);
    }
}

/// `rgb` turned red: the same lightness and saturation, the hue moved to
/// red (0x0067c0, Windows blue, becomes 0xc01f00).
fn as_red(rgb: u32) -> u32 {
    let [r, g, b] = [(rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff].map(|c| c as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let s = if max == min { 0.0 } else { (max - min) / (1.0 - (2.0 * l - 1.0).abs()) };
    // HSL back to RGB with the hue at red (0 degrees, nudged a little
    // toward orange so it reads as red rather than crimson).
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h: f32 = 8.0 / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u32;
    byte(c) << 16 | byte(x) << 8 | byte(0.0)
}
