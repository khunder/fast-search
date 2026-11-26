//! Reading PNG images, for icon themes (Linux): zlib inflate and the PNG
//! filters, in the standard library only. Covers what icons use: 8-bit
//! greyscale, RGB, palette, grey + alpha and RGBA, not interlaced; 16-bit
//! channels are cut to 8. Anything else is refused (None), never trusted:
//! all sizes are checked.

#![cfg_attr(windows, allow(dead_code))] // Windows asks the shell instead

/// An image as BGRA pixels (alpha not premultiplied), row by row.
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<[u8; 4]>,
}

pub fn decode(data: &[u8]) -> Option<Image> {
    if data.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let (mut w, mut h, mut depth, mut kind, mut interlace) = (0usize, 0usize, 0u8, 0u8, 0u8);
    let (mut palette, mut trns, mut idat) = (Vec::new(), Vec::new(), Vec::new());
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().ok()?) as usize;
        let name = &data[at + 4..at + 8];
        let body = data.get(at + 8..at + 8 + len)?;
        match name {
            b"IHDR" if len >= 13 => {
                w = u32::from_be_bytes(body[0..4].try_into().ok()?) as usize;
                h = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
                (depth, kind, interlace) = (body[8], body[9], body[12]);
            }
            b"PLTE" => palette = body.to_vec(),
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    if w == 0 || h == 0 || w > 4096 || h > 4096 || interlace != 0 || !(depth == 8 || depth == 16) {
        return None;
    }
    let channels = match kind {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    if kind == 3 && depth != 8 {
        return None;
    }
    let bpp = channels * depth as usize / 8;
    let stride = w * bpp;
    let raw = inflate(idat.get(2..)?, h * (stride + 1))?;
    if raw.len() < h * (stride + 1) {
        return None;
    }
    // Undo the per-row filters.
    let mut rows = vec![0u8; h * stride];
    for y in 0..h {
        let filter = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let (done, rest) = rows.split_at_mut(y * stride);
        let prev = if y > 0 { &done[(y - 1) * stride..] } else { &[][..] };
        let cur = &mut rest[..stride];
        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
            let b = prev.get(i).copied().unwrap_or(0) as i32;
            let c = if i >= bpp { prev.get(i - bpp).copied().unwrap_or(0) as i32 } else { 0 };
            let x = line[i] as i32;
            cur[i] = match filter {
                0 => x,
                1 => x + a,
                2 => x + b,
                3 => x + (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    x + if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                }
                _ => return None,
            } as u8;
        }
    }
    let sample = |i: usize| rows[i * depth as usize / 8]; // the high byte of 16-bit samples
    let mut px = Vec::with_capacity(w * h);
    for p in 0..w * h {
        let s = |k: usize| sample(p * channels + k);
        let [r, g, b, a] = match kind {
            0 => [s(0), s(0), s(0), 255],
            2 => [s(0), s(1), s(2), 255],
            3 => {
                let i = s(0) as usize;
                let rgb = palette.get(i * 3..i * 3 + 3)?;
                [rgb[0], rgb[1], rgb[2], trns.get(i).copied().unwrap_or(255)]
            }
            4 => [s(0), s(0), s(0), s(1)],
            _ => [s(0), s(1), s(2), s(3)],
        };
        px.push([b, g, r, a]);
    }
    Some(Image { w, h, px })
}

/// Inflates a raw deflate stream (zlib's two header bytes already
/// skipped), stopping at `limit` bytes.
fn inflate(src: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut bits = Bits { src, pos: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(limit);
    loop {
        let last = bits.get(1)?;
        match bits.get(2)? {
            0 => {
                bits.pos = bits.pos.div_ceil(8) * 8;
                let at = bits.pos / 8;
                let len = u16::from_le_bytes(src.get(at..at + 2)?.try_into().ok()?) as usize;
                out.extend_from_slice(src.get(at + 4..at + 4 + len)?);
                bits.pos = (at + 4 + len) * 8;
            }
            1 => {
                let mut lit = [8u8; 288];
                lit[144..256].fill(9);
                lit[256..280].fill(7);
                block(&mut bits, &mut out, &Huffman::new(&lit)?, &Huffman::new(&[5; 30])?, limit)?;
            }
            2 => {
                let (hlit, hdist, hclen) = (bits.get(5)? as usize + 257, bits.get(5)? as usize + 1, bits.get(4)? as usize + 4);
                const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut code_lens = [0u8; 19];
                for &i in ORDER.iter().take(hclen) {
                    code_lens[i] = bits.get(3)? as u8;
                }
                let codes = Huffman::new(&code_lens)?;
                let mut lens = Vec::with_capacity(hlit + hdist);
                while lens.len() < hlit + hdist {
                    match codes.read(&mut bits)? {
                        n @ 0..=15 => lens.push(n as u8),
                        16 => {
                            let prev = *lens.last()?;
                            let n = 3 + bits.get(2)?;
                            lens.extend(std::iter::repeat_n(prev, n as usize));
                        }
                        17 => lens.extend(std::iter::repeat_n(0, 3 + bits.get(3)? as usize)),
                        _ => lens.extend(std::iter::repeat_n(0, 11 + bits.get(7)? as usize)),
                    }
                }
                let (lit, dist) = lens.split_at(hlit);
                block(&mut bits, &mut out, &Huffman::new(lit)?, &Huffman::new(dist)?, limit)?;
            }
            _ => return None,
        }
        if last == 1 || out.len() >= limit {
            return Some(out);
        }
    }
}

fn block(bits: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, limit: usize) -> Option<()> {
    const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DIST_BASE: [u16; 30] =
        [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
    const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    loop {
        let sym = lit.read(bits)? as usize;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Some(()),
            _ => {
                let i = sym - 257;
                let len = *LEN_BASE.get(i)? as usize + bits.get(*LEN_EXTRA.get(i)? as u32)? as usize;
                let d = dist.read(bits)? as usize;
                let back = *DIST_BASE.get(d)? as usize + bits.get(*DIST_EXTRA.get(d)? as u32)? as usize;
                if back > out.len() {
                    return None;
                }
                for _ in 0..len {
                    out.push(out[out.len() - back]);
                }
            }
        }
        if out.len() > limit + 258 {
            return Some(()); // more than the image needs: the rest is not read
        }
    }
}

struct Bits<'a> {
    src: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    /// `n` bits, lowest first.
    fn get(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for k in 0..n {
            let byte = *self.src.get(self.pos / 8)?;
            v |= ((byte >> (self.pos % 8)) as u32 & 1) << k;
            self.pos += 1;
        }
        Some(v)
    }
}

/// A canonical Huffman code, by code length.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lens: &[u8]) -> Option<Huffman> {
        let mut counts = [0u16; 16];
        for &l in lens {
            *counts.get_mut(l as usize)? += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for l in 1..16 {
            offsets[l] = offsets[l - 1] + counts[l - 1];
        }
        let mut symbols = vec![0u16; lens.len()];
        for (s, &l) in lens.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = s as u16;
                offsets[l as usize] += 1;
            }
        }
        Some(Huffman { counts, symbols })
    }

    fn read(&self, bits: &mut Bits) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.get(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return self.symbols.get((index + code - first) as usize).copied();
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own encoder's output (fixed codes, runs) decodes back.
    #[test]
    fn decodes_the_app_icon() {
        let ico = crate::app_icon::ico();
        // The 256-pixel frame is the last of the .ico file.
        let n = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        let e = 6 + 16 * (n - 1);
        let len = u32::from_le_bytes(ico[e + 8..e + 12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().unwrap()) as usize;
        let image = decode(&ico[off..off + len]).unwrap();
        assert_eq!((image.w, image.h), (256, 256));
        let expected = crate::app_icon::render(256, crate::app_icon::sample);
        assert_eq!(image.px, expected);
    }
    /// Real icons, as themes ship them (dynamic Huffman codes, filters,
    /// palettes): every 16 by 16 PNG installed decodes to 16 by 16.
    #[test]
    #[cfg(target_os = "linux")]
    fn decodes_installed_icons() {
        let Ok(themes) = std::fs::read_dir("/usr/share/icons") else { return };
        let mut decoded = 0;
        for theme in themes.flatten() {
            let Ok(dirs) = std::fs::read_dir(theme.path().join("16x16")) else { continue };
            for dir in dirs.flatten() {
                for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten().take(40) {
                    let Ok(data) = std::fs::read(file.path()) else { continue };
                    if file.path().extension().is_some_and(|e| e == "png") && !file.path().is_symlink() {
                        let image = decode(&data).unwrap_or_else(|| panic!("{}", file.path().display()));
                        assert_eq!((image.w, image.h), (16, 16), "{}", file.path().display());
                        decoded += 1;
                    }
                }
            }
        }
        eprintln!("decoded {decoded} installed icons");
    }


    #[test]
    fn refuses_garbage() {
        assert!(decode(b"not a png").is_none());
        assert!(decode(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR").is_none());
    }
}
