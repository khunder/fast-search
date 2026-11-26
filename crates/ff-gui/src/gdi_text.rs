//! Text on Windows, drawn by GDI with ClearType: the same sharp, hinted,
//! sub-pixel text as every other Windows program, in the system font
//! (Segoe UI Variable on Windows 11, Segoe UI before). It has the same
//! interface as the portable rasterizer in `canvas.rs`.
//!
//! Each string is drawn into a small bitmap that first receives the
//! canvas pixels under it, so ClearType blends with what is really there,
//! then copied back.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::io;

use crate::canvas::{Canvas, tone};

type HDC = *mut c_void;
type HGDIOBJ = *mut c_void;

#[repr(C)]
struct SIZE {
    cx: i32,
    cy: i32,
}

#[repr(C)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
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

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: HDC) -> HDC;
    fn DeleteDC(dc: HDC) -> i32;
    fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike_out: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch_and_family: u32,
        face: *const u16,
    ) -> HGDIOBJ;
    fn SelectObject(dc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
    fn DeleteObject(obj: HGDIOBJ) -> i32;
    fn GetTextFaceW(dc: HDC, len: i32, face: *mut u16) -> i32;
    fn GetTextMetricsW(dc: HDC, metrics: *mut u8) -> i32;
    fn GetTextExtentPoint32W(dc: HDC, s: *const u16, len: i32, size: *mut SIZE) -> i32;
    fn GetTextExtentExPointW(dc: HDC, s: *const u16, len: i32, max: i32, fit: *mut i32, dx: *mut i32, size: *mut SIZE) -> i32;
    fn CreateDIBSection(dc: HDC, info: *const BITMAPINFOHEADER, usage: u32, bits: *mut *mut u8, section: *mut c_void, offset: u32) -> HGDIOBJ;
    fn SetTextColor(dc: HDC, color: u32) -> u32;
    fn SetBkMode(dc: HDC, mode: i32) -> i32;
    fn ExtTextOutW(dc: HDC, x: i32, y: i32, options: u32, rect: *const RECT, s: *const u16, len: u32, dx: *const i32) -> i32;
    fn GdiFlush() -> i32;
}

const ELLIPSIS: u16 = 0x2026;

pub struct Text {
    dc: HDC,
    font: HGDIOBJ,
    /// The scratch bitmap strings are drawn into, its pixels and size.
    bitmap: HGDIOBJ,
    bits: *mut u8,
    cap: (i32, i32),
    pub height: i32,
}

impl Text {
    /// The system UI font at `px` pixels (the em height).
    pub fn load(px: f32) -> io::Result<Text> {
        // SAFETY: plain GDI calls on objects created here; every pointer
        // passed points at a live, properly sized buffer.
        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                return Err(io::Error::last_os_error());
            }
            const CLEARTYPE_QUALITY: u32 = 5;
            const DEFAULT_CHARSET: u32 = 1;
            let mut font = std::ptr::null_mut();
            // The first face GDI really has; it substitutes silently, so
            // check the name it took.
            for face in ["Segoe UI Variable Text", "Segoe UI"] {
                let name: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
                let f = CreateFontW(-(px.round() as i32), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, 0, 0, CLEARTYPE_QUALITY, 0, name.as_ptr());
                if f.is_null() {
                    continue;
                }
                let old = SelectObject(dc, f);
                let mut got = [0u16; 64];
                let n = GetTextFaceW(dc, got.len() as i32, got.as_mut_ptr());
                if String::from_utf16_lossy(&got[..(n.max(1) - 1) as usize]).eq_ignore_ascii_case(face) {
                    font = f;
                    break;
                }
                SelectObject(dc, old);
                DeleteObject(f);
            }
            if font.is_null() {
                DeleteDC(dc);
                return Err(io::Error::other("the Segoe UI font is missing"));
            }
            // TEXTMETRICW starts with the line height.
            let mut metrics = [0u8; 64];
            GetTextMetricsW(dc, metrics.as_mut_ptr());
            let height = i32::from_le_bytes(metrics[0..4].try_into().unwrap());

            const TRANSPARENT: i32 = 1;
            SetBkMode(dc, TRANSPARENT);
            Ok(Text { dc, font, bitmap: std::ptr::null_mut(), bits: std::ptr::null_mut(), cap: (0, 0), height })
        }
    }

    fn extent(&self, s: &[u16]) -> i32 {
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: a valid string and its length.
        unsafe { GetTextExtentPoint32W(self.dc, s.as_ptr(), s.len() as i32, &mut size) };
        size.cx
    }

    /// The advance after each UTF-16 unit of `s`.
    fn advances(&self, s: &[u16]) -> Vec<i32> {
        let mut dx = vec![0i32; s.len()];
        let (mut fit, mut size) = (0, SIZE { cx: 0, cy: 0 });
        // SAFETY: `dx` has room for one value per unit.
        unsafe { GetTextExtentExPointW(self.dc, s.as_ptr(), s.len() as i32, i32::MAX, &mut fit, dx.as_mut_ptr(), &mut size) };
        dx
    }

    pub fn width(&mut self, s: &str) -> i32 {
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.extent(&wide)
    }

    /// Byte index in `s` of the character boundary nearest to offset `x`.
    pub fn index_at(&mut self, s: &str, x: i32) -> usize {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let dx = self.advances(&wide);
        let mut unit = 0;
        for (byte, ch) in s.char_indices() {
            let start = if unit == 0 { 0 } else { dx[unit - 1] };
            unit += ch.len_utf16();
            let end = dx[unit - 1];
            if x < (start + end) / 2 {
                return byte;
            }
        }
        s.len()
    }

    /// Makes the scratch bitmap at least `w` x `h`.
    fn reserve(&mut self, w: i32, h: i32) -> bool {
        if w <= self.cap.0 && h <= self.cap.1 && !self.bits.is_null() {
            return true;
        }
        let (w, h) = (w.max(self.cap.0).max(256), h.max(self.cap.1).max(32));
        let info = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down rows, like the canvas
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: a filled-in header; GDI returns the pixel memory.
        unsafe {
            let bitmap = CreateDIBSection(self.dc, &info, 0, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() {
                return false;
            }
            SelectObject(self.dc, bitmap);
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            self.bitmap = bitmap;
        }
        self.bits = bits;
        self.cap = (w, h);
        true
    }

    /// Draws `s` with its top-left at (`x`, `top`), shortened with an
    /// ellipsis if it is wider than `max_w`.
    pub fn draw(&mut self, cv: &mut Canvas, x: i32, top: i32, s: &str, max_w: i32, color: u32) {
        if s.is_empty() || max_w <= 0 {
            return;
        }
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        if self.extent(&wide) > max_w {
            let room = max_w - self.extent(&[ELLIPSIS]);
            let dx = self.advances(&wide);
            let mut keep = dx.iter().take_while(|&&a| a <= room).count();
            // Never split a surrogate pair.
            if keep > 0 && (0xD800..0xDC00).contains(&wide[keep - 1]) {
                keep -= 1;
            }
            wide.truncate(keep);
            wide.push(ELLIPSIS);
        }
        // The part of the canvas the text may touch (ClearType reaches a
        // pixel or two past the advance).
        let width = self.extent(&wide).min(max_w);
        let x0 = x.max(cv.clip.0).max(0);
        let x1 = (x + width + 2).min(cv.clip.1).min(cv.w as i32);
        let (y0, y1) = (top.max(0), (top + self.height).min(cv.h as i32));
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0 || h <= 0 || !self.reserve(w, h) {
            return;
        }
        let stride = self.cap.0 as usize * 4;
        let row = w as usize * 4;
        // SAFETY: the bitmap is at least w x h; rows are copied within
        // both buffers' bounds.
        unsafe {
            let bits = std::slice::from_raw_parts_mut(self.bits, stride * self.cap.1 as usize);
            for r in 0..h as usize {
                let from = ((y0 as usize + r) * cv.w + x0 as usize) * 4;
                bits[r * stride..r * stride + row].copy_from_slice(&cv.px[from..from + row]);
            }
            let c = tone(color);
            // COLORREF is 0x00BBGGRR.
            SetTextColor(self.dc, (c >> 16 & 0xff) | (c & 0xff00) | (c & 0xff) << 16);
            const ETO_CLIPPED: u32 = 4;
            let clip = RECT { left: 0, top: 0, right: w, bottom: h };
            ExtTextOutW(self.dc, x - x0, top - y0, ETO_CLIPPED, &clip, wide.as_ptr(), wide.len() as u32, std::ptr::null());
            GdiFlush();
            for r in 0..h as usize {
                let to = ((y0 as usize + r) * cv.w + x0 as usize) * 4;
                cv.px[to..to + row].copy_from_slice(&bits[r * stride..r * stride + row]);
            }
        }
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        // SAFETY: objects created by this value and not used after.
        unsafe {
            DeleteDC(self.dc);
            DeleteObject(self.font);
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
        }
    }
}
