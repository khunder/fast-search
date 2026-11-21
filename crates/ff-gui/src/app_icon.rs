//! The application icon, an eye, drawn from shapes; and the same icon as
//! a Windows `.ico` file and icon resource.
//!
//! This file depends on nothing but the standard library: the window
//! draws the icon with it, and the build script embeds the icon in
//! `ff-gui.exe` with it (`#[path]`-included there), so both always agree.

#![allow(dead_code)] // each user takes only part of it

/// The icon's colour (0xRRGGBB) at a point of the unit square, if any.
pub fn sample(u: f32, v: f32) -> Option<u32> {
    // An almond: the overlap of two circles, one centred below and one
    // above the middle. `r` shrinks it for the outline.
    let almond = |r: f32| {
        let dx2 = (u - 0.5).powi(2);
        dx2 + (v - 0.74).powi(2) <= r * r && dx2 + (v - 0.26).powi(2) <= r * r
    };
    if !almond(0.51) {
        return None;
    }
    if !almond(0.455) {
        return Some(0xf08a1c); // eyelid outline
    }
    let from = |cx: f32, cy: f32| ((u - cx).powi(2) + (v - cy).powi(2)).sqrt();
    Some(if from(0.555, 0.445) <= 0.035 {
        0xffffff // glint
    } else if from(0.5, 0.5) <= 0.085 {
        0x2b2118 // pupil
    } else if from(0.5, 0.5) <= 0.17 {
        0xc96a12 // iris
    } else {
        0xfdfdfd
    })
}

/// Renders a shape as `size` x `size` BGRA pixels (alpha not
/// premultiplied), with 4x4 supersampling for smooth edges.
pub fn render(size: usize, sample: impl Fn(f32, f32) -> Option<u32>) -> Vec<[u8; 4]> {
    const N: usize = 4;
    let step = 1.0 / (size * N) as f32;
    (0..size * size)
        .map(|i| {
            let (px, py) = (i % size, i / size);
            let mut sum = [0u32; 4];
            for s in 0..N * N {
                let u = ((px * N + s % N) as f32 + 0.5) * step;
                let v = ((py * N + s / N) as f32 + 0.5) * step;
                if let Some(c) = sample(u, v) {
                    let [b, g, r] = [c as u8, (c >> 8) as u8, (c >> 16) as u8];
                    sum = [sum[0] + b as u32, sum[1] + g as u32, sum[2] + r as u32, sum[3] + 1];
                }
            }
            let n = sum[3].max(1);
            [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, (sum[3] * 255 / (N * N) as u32) as u8]
        })
        .collect()
}

/// The sizes Windows picks from, for lists, the taskbar and Explorer.
const SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Each size as a PNG image.
fn images() -> Vec<(usize, Vec<u8>)> {
    SIZES.iter().map(|&size| (size, png(size, &render(size, sample)))).collect()
}

/// The icon as an `.ico` file.
pub fn ico() -> Vec<u8> {
    let images = images();
    let mut out = Vec::new();
    out.extend([0u16, 1, images.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * images.len();
    for (size, data) in &images {
        out.extend([*size as u8, *size as u8, 0, 0]); // 256 is written as 0
        out.extend(1u16.to_le_bytes()); // planes
        out.extend(32u16.to_le_bytes()); // bits per pixel
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((offset as u32).to_le_bytes());
        offset += data.len();
    }
    images.iter().for_each(|(_, data)| out.extend(data));
    out
}

/// The icon as a compiled Windows resource file (`.res`), which the
/// linker takes like an object file. The group has the lowest number, so
/// Explorer shows it as the program's icon.
pub fn windows_resource() -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    fn entry(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(32u32.to_le_bytes()); // header size
        out.extend([0xFFFF, kind, 0xFFFF, id].iter().flat_map(|v: &u16| v.to_le_bytes()));
        out.extend(0u32.to_le_bytes()); // data version
        out.extend(0x1010u16.to_le_bytes()); // moveable, discardable
        out.extend(0u16.to_le_bytes()); // language: neutral
        out.extend(0u64.to_le_bytes()); // version, characteristics
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    let mut out = Vec::new();
    entry(&mut out, 0, 0, &[]); // every .res file starts with an empty entry
    let images = images();
    let mut group: Vec<u8> = [0u16, 1, images.len() as u16].iter().flat_map(|v| v.to_le_bytes()).collect();
    for (i, (size, data)) in images.iter().enumerate() {
        let id = i as u16 + 1;
        entry(&mut out, RT_ICON, id, data);
        group.extend([*size as u8, *size as u8, 0, 0]);
        group.extend(1u16.to_le_bytes());
        group.extend(32u16.to_le_bytes());
        group.extend((data.len() as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, &group);
    out
}

// ---- PNG ---------------------------------------------------------------------

/// Encodes BGRA pixels as an RGBA PNG.
fn png(size: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(size * (size * 4 + 1));
    for row in pixels.chunks_exact(size) {
        raw.push(0); // no filter
        raw.extend(row.iter().flat_map(|&[b, g, r, a]| [r, g, b, a]));
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend((size as u32).to_be_bytes());
    header.extend((size as u32).to_be_bytes());
    header.extend([8, 6, 0, 0, 0]); // 8 bits, RGBA
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

/// Deflate with the fixed Huffman codes, matching only a repeat of the
/// previous pixel (4 bytes back): enough for an icon made of flat
/// colours, and small.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut w = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
    w.put(1, 1); // last block
    w.put(1, 2); // fixed codes
    let mut i = 0;
    while i < data.len() {
        let run = if i >= 4 { (0..258.min(data.len() - i)).take_while(|&k| data[i + k] == data[i + k - 4]).count() } else { 0 };
        if run >= 3 {
            w.length(run);
            w.code(3, 5); // distance 4: code 3, no extra bits
            i += run;
        } else {
            w.literal(data[i] as u16);
            i += 1;
        }
    }
    w.literal(256); // end of block
    if w.n > 0 {
        w.out.push(w.acc as u8);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    w.out.extend((b << 16 | a).to_be_bytes());
    w.out
}

struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    /// Writes `count` bits of `value`, lowest first.
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= value << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Writes a Huffman code, which goes highest bit first.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn literal(&mut self, symbol: u16) {
        match symbol {
            0..=143 => self.code(0x30 + symbol as u32, 8),
            144..=255 => self.code(0x190 + (symbol as u32 - 144), 9),
            256..=279 => self.code(symbol as u32 - 256, 7),
            _ => self.code(0xC0 + (symbol as u32 - 280), 8),
        }
    }

    /// Writes a match length (3 to 258).
    fn length(&mut self, len: usize) {
        const BASE: [usize; 29] =
            [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
        const EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
        let i = BASE.iter().rposition(|&b| b <= len).unwrap();
        self.literal(257 + i as u16);
        self.put((len - BASE[i]) as u32, EXTRA[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_files_are_well_formed() {
        let ico = ico();
        assert_eq!(&ico[..6], &[0, 0, 1, 0, SIZES.len() as u8, 0]);
        let first = u32::from_le_bytes(ico[18..22].try_into().unwrap()) as usize;
        assert_eq!(&ico[first..first + 8], b"\x89PNG\r\n\x1a\n");
        let res = windows_resource();
        assert_eq!(&res[..8], &[0, 0, 0, 0, 32, 0, 0, 0]);
        assert_eq!(res.len() % 4, 0);
    }

    /// The deflate stream decodes back to its input (with a small
    /// inflater for fixed-code blocks).
    #[test]
    fn deflate_round_trips() {
        let mut data = vec![0u8; 2000];
        data.extend((0..300).map(|i| (i * 7) as u8));
        data.extend([1, 2, 3, 4].repeat(200));
        let z = zlib(&data);
        assert_eq!(inflate_fixed(&z[2..z.len() - 4]), data);
    }

    fn inflate_fixed(src: &[u8]) -> Vec<u8> {
        let mut pos = 0usize;
        let mut bit = |n: u32| -> u32 {
            let mut v = 0;
            for k in 0..n {
                v |= ((src[pos / 8] >> (pos % 8)) as u32 & 1) << k;
                pos += 1;
            }
            v
        };
        assert_eq!(bit(3), 0b011);
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Read a fixed Huffman literal/length symbol, highest bit first.
            let mut code = 0u32;
            let mut len = 0;
            let sym = loop {
                code = code << 1 | bit(1);
                len += 1;
                match (len, code) {
                    (7, 0..=0x17) => break 256 + code,
                    (8, 0x30..=0xBF) => break code - 0x30,
                    (8, 0xC0..=0xC7) => break 280 + code - 0xC0,
                    (9, 0x190..=0x1FF) => break 144 + code - 0x190,
                    _ => {}
                }
            };
            match sym {
                0..=255 => out.push(sym as u8),
                256 => return out,
                _ => {
                    const BASE: [usize; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
                    const EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
                    let i = (sym - 257) as usize;
                    let len = BASE[i] + bit(EXTRA[i]) as usize;
                    let mut d = 0;
                    for _ in 0..5 {
                        d = d << 1 | bit(1);
                    }
                    assert_eq!(d, 3);
                    for _ in 0..len {
                        out.push(out[out.len() - 4]);
                    }
                }
            }
        }
    }
}
