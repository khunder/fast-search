//! Completion in the search box: Alt+Down lists the filters (`ext:`,
//! `size:`, `modified:`) and, after one of them, values to pick from.
//! The list follows what is typed while it is open.

use ff_core::complete::suggestions;

use crate::backend::{Key, Mods};
use crate::i18n::tr;
use crate::keys::*;
use crate::{App, DIM, HOVER, TEXT, WHITE};

/// The open list.
pub struct Complete {
    /// What each entry puts in place of the word, and what it means.
    items: Vec<(String, &'static str)>,
    chosen: usize,
    /// Where the word being completed starts in the search text.
    start: usize,
}

impl App {
    /// The word around the caret: (start, end) in the search text.
    fn word_at_caret(&self) -> (usize, usize) {
        let start = self.query[..self.cursor].rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let end = self.query[self.cursor..].find(char::is_whitespace).map_or(self.query.len(), |i| self.cursor + i);
        (start, end)
    }

    /// Opens the list for the word at the caret, or refreshes it after
    /// the text changed; closes it when nothing fits.
    pub(crate) fn update_completion(&mut self) {
        let (start, _) = self.word_at_caret();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let (y, m, d, ..) = ff_core::local_civil(now);
        let items = suggestions(&self.query[start..self.cursor], (y, m, d));
        // Keep the same entry chosen while typing narrows the list.
        let before = self.complete.as_ref().and_then(|c| c.items.get(c.chosen)).map(|(text, _)| text.clone());
        let chosen = before.and_then(|text| items.iter().position(|(t, _)| *t == text)).unwrap_or(0);
        self.complete = (!items.is_empty()).then_some(Complete { items, chosen, start });
    }

    /// Puts the chosen entry in place of the word at the caret.
    fn accept_completion(&mut self, item: usize) {
        let Some(complete) = self.complete.take() else { return };
        let Some((text, _)) = complete.items.get(item) else { return };
        let (_, end) = self.word_at_caret();
        (self.anchor, self.cursor) = (complete.start, end);
        // A filter name is followed by its values; a finished filter
        // by a space, ready for the next word.
        let names_filter = text.ends_with(':');
        let more = if names_filter || self.query[end..].starts_with(char::is_whitespace) { "" } else { " " };
        self.insert(&format!("{text}{more}"));
        if names_filter {
            self.update_completion();
        }
    }

    /// A key while the list is open; false leaves it to the search box.
    pub(crate) fn completion_key(&mut self, key: Key, mods: Mods) -> bool {
        let Some(complete) = self.complete.as_mut() else { return false };
        let n = complete.items.len();
        match key {
            Key::Sym(K_UP) => complete.chosen = (complete.chosen + n - 1) % n,
            Key::Sym(K_DOWN) if !mods.alt => complete.chosen = (complete.chosen + 1) % n,
            Key::Sym(K_RETURN | K_KP_ENTER | K_TAB) => {
                let chosen = complete.chosen;
                self.accept_completion(chosen);
            }
            Key::Sym(K_ESCAPE) => self.complete = None,
            Key::Sym(K_DOWN) => {}
            _ => return false,
        }
        true
    }

    /// (x, y, width, height) of the list, under the word it completes.
    fn completion_rect(&mut self) -> Option<(i32, i32, i32, i32)> {
        let complete = self.complete.take()?;
        let widest = complete.items.iter().map(|(text, means)| self.text.width(text) + self.text.width(tr(means))).max().unwrap_or(0);
        let w = widest + 52 * self.u;
        let h = complete.items.len() as i32 * self.row_h() + 8 * self.u;
        let x = self.box_text_x() + self.text.width(&self.query[..complete.start]) - 10 * self.u;
        let rect = (x.min(self.cv.w as i32 - w - 4 * self.u).max(4 * self.u), self.box_h() - 5 * self.u, w, h);
        self.complete = Some(complete);
        Some(rect)
    }

    fn completion_item_at(&mut self, px: i32, py: i32) -> Option<usize> {
        let (x, y, w, _) = self.completion_rect()?;
        let item = (py - y - 4 * self.u).div_euclid(self.row_h());
        let n = self.complete.as_ref()?.items.len() as i32;
        (px >= x && px < x + w && (0..n).contains(&item)).then_some(item as usize)
    }

    /// A press while the list is open: on an entry it is chosen, anywhere
    /// else the list closes. True if the press was for the list.
    pub(crate) fn completion_press(&mut self, button: u8, x: i32, y: i32) -> bool {
        if self.complete.is_none() {
            return false;
        }
        match self.completion_item_at(x, y) {
            Some(item) if button == 1 => self.accept_completion(item),
            Some(_) => {}
            None => {
                self.complete = None;
                return false;
            }
        }
        true
    }

    /// The pointer moved; true if another entry is now under it.
    pub(crate) fn completion_motion(&mut self, x: i32, y: i32) -> bool {
        let Some(item) = self.completion_item_at(x, y) else { return false };
        self.complete.as_mut().is_some_and(|c| std::mem::replace(&mut c.chosen, item) != item)
    }

    pub(crate) fn draw_completion(&mut self) {
        let Some((x, y, w, h)) = self.completion_rect() else { return };
        let Some(complete) = self.complete.take() else { return };
        let (u, row_h) = (self.u, self.row_h());
        self.cv.fill(x + 3 * u, y + 3 * u, w, h, 0xbdbdbd); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x9a9a9a);
        for (i, (text, means)) in complete.items.iter().enumerate() {
            let iy = y + 4 * u + i as i32 * row_h;
            if i == complete.chosen {
                self.cv.fill(x + u, iy, w - 2 * u, row_h, HOVER);
            }
            self.text.draw(&mut self.cv, x + 10 * u, iy + 3 * u, text, w, TEXT);
            let means = tr(means);
            let mx = x + w - 10 * u - self.text.width(means);
            self.text.draw(&mut self.cv, mx, iy + 3 * u, means, w, DIM);
        }
        self.complete = Some(complete);
    }
}
