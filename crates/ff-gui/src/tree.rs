//! The folder sizes view inside the main window: the tree on the left,
//! the map of the selected folder on the right (the data is in
//! `ff_core::sizes`).

use std::io;

use ff_core::group_digits;
use ff_core::sizes::human_size as units;
pub use ff_core::sizes::{Kept, Tile, Tree};

/// "% of disk" bars: the accent, as for "% of parent"...
const DISK_OK: u32 = crate::SELECT;
/// ... and a red of the same tone once a drive is 90 % full, as
/// Explorer warns of full drives.
const DISK_FULL: u32 = crate::canvas::ACCENT_RED;

use crate::backend::{Key, Mods};
use crate::canvas::{ON_ACCENT, palette};
use crate::i18n::{tr, trf};
use crate::keys::*;
use crate::{Action, App, DIM, LINE, Menu, PANEL, SELECT, TEXT, WHITE};

// ---- the view, inside the main window ----------------------------------

impl App {
    /// Opens the folder sizes view, or goes back to the search.
    pub(crate) fn toggle_tree(&mut self) {
        self.tiles.clear();
        self.tile_hover = None;
        if self.tree.take().is_some() {
            self.top = self.top.min(self.max_top());
            return;
        }
        let mut tree = Tree::new(&self.index);
        // Coming from a search, start at the selected result.
        if let Some(&id) = self.hits.get(self.sel).filter(|_| !self.query.trim().is_empty()) {
            if tree.reveal(&self.index, id) {
                tree.open(tree.sel);
            }
        }
        self.tree = Some(tree);
        self.measure_disks();
        self.show_tree_sel();
    }

    /// Asks the system for the capacity of each indexed drive, for the
    /// "% of disk" column.
    fn measure_disks(&mut self) {
        let index = &self.index;
        let disks = index.roots().filter_map(|r| ff_core::os::disk_space(&index.path(r)).map(|d| (r, d))).collect();
        self.disks = disks;
    }

    /// The capacity and free space of the drive `id` is on, if known.
    fn disk_of(&self, mut id: u32) -> Option<(u64, u64)> {
        while let Some(p) = self.index.parent(id) {
            id = p;
        }
        self.disks.iter().find(|d| d.0 == id).map(|d| d.1)
    }

    pub(crate) fn keep_tree(&self) -> Option<Kept> {
        self.tree.as_ref().map(|t| t.keep(&self.index))
    }

    /// Recomputes the view after the index changed.
    pub(crate) fn restore_tree(&mut self, kept: Option<Kept>) {
        if let Some(kept) = kept {
            self.tree = Some(Tree::restored(&self.index, &kept));
            self.measure_disks();
            let max = self.max_top();
            if let Some(tree) = self.tree.as_mut() {
                tree.top = tree.top.min(max);
            }
        }
    }

    /// Runs `f` as if the tree's selected entry were the only search result,
    /// so the actions on results work on it.
    pub(crate) fn on_tree_target<R>(&mut self, f: impl FnOnce(&mut App) -> R) -> Option<R> {
        let id = self.tree.as_ref()?.selected()?;
        let tree = self.tree.take();
        let saved = (std::mem::replace(&mut self.hits, vec![id]), self.sel, std::mem::take(&mut self.marks));
        self.sel = 0;
        let result = f(self);
        (self.hits, self.sel, self.marks) = saved;
        // The action may have changed the index and rebuilt nothing: the
        // callers that replace the index restore the tree themselves.
        if self.tree.is_none() {
            self.tree = tree;
        }
        Some(result)
    }

    /// Scrolls the tree so its selected row is visible.
    fn show_tree_sel(&mut self) {
        let rows = self.rows_visible();
        if let Some(tree) = self.tree.as_mut() {
            tree.sel = tree.sel.min(tree.rows.len().saturating_sub(1));
            if tree.sel < tree.top {
                tree.top = tree.sel;
            } else if tree.sel >= tree.top + rows {
                tree.top = tree.sel + 1 - rows;
            }
            tree.top = tree.top.min(tree.rows.len().saturating_sub(rows));
        }
    }

    /// Where the tree ends and the map starts (the window's width when
    /// it is too narrow for a map).
    fn tree_split(&self) -> i32 {
        let w = self.cv.w as i32;
        // The map can be turned off (View > Size map, F7): the tree then
        // takes the whole width.
        if w < 640 * self.u || !self.flag(crate::Flag::SizeMap) { w } else { (w * 46 / 100).max(380 * self.u) }
    }

    /// (x, width) of the Name, Size, Files, Folders, % of parent, % of disk,
    /// Free and Modified columns. Narrow trees (beside the map) drop
    /// columns from the right; % of disk and Free are left out where drive
    /// sizes are unknown.
    fn tree_columns(&mut self) -> [(i32, i32); 8] {
        let pad = 8 * self.u;
        let count = self.text.width("999.9K").max(self.text.width(tr("Folders"))) + 2 * pad;
        let size = self.text.width("1,023.9 MB") + 2 * pad;
        let share = [tr("% of parent"), tr("% of disk")].iter().map(|h| self.text.width(h)).max().unwrap_or(0).max(84 * self.u) + 2 * pad;
        let date = self.text.width("0000-00-00") + 2 * pad;
        let room = self.tree_split() - self.bar_w();
        let (disk, free) = if self.disks.is_empty() { (0, 0) } else { (share, size) };
        let mut widths = [0, size, count, count, share, disk, free, date];
        // Columns go from the right end while the name keeps 200 units.
        for i in [7, 6, 5, 3, 2] {
            if room - widths.iter().sum::<i32>() < 200 * self.u {
                widths[i] = 0;
            }
        }
        widths[0] = (room - widths.iter().sum::<i32>()).max(120 * self.u);
        let mut x = 0;
        widths.map(|w| {
            let col = (x, w);
            x += w;
            col
        })
    }

    fn tree_row_at(&self, x: i32, y: i32) -> Option<usize> {
        let tree = self.tree.as_ref()?;
        let row = tree.top + ((y - self.list_top()) / self.row_h()) as usize;
        (x < self.tree_split() - self.bar_w() && y >= self.list_top() && row < tree.rows.len() && row < tree.top + self.rows_visible()).then_some(row)
    }

    /// The innermost tile of the map at a window position.
    fn tile_at(&self, x: i32, y: i32) -> Option<Tile> {
        self.tiles.iter().rev().find(|t| x >= t.x && x < t.x + t.w && y >= t.y && y < t.y + t.h).copied()
    }

    /// The folder the map shows: the selected one, or the one holding
    /// the selected file.
    fn map_root(&self, tree: &Tree) -> Option<u32> {
        let id = tree.selected()?;
        if self.index.is_dir(id) { Some(id) } else { self.index.parent(id) }
    }

    fn select_in_tree(&mut self, id: u32) {
        if let Some(tree) = self.tree.as_mut() {
            tree.reveal(&self.index, id);
        }
        self.show_tree_sel();
    }

    /// A key while the view is open; `None` leaves it to the search window.
    pub(crate) fn tree_key(&mut self, key: Key, mods: Mods) -> Option<io::Result<bool>> {
        let page = self.rows_visible();
        let tree = self.tree.as_mut()?;
        let last = tree.rows.len().saturating_sub(1);
        match key {
            Key::Sym(K_ESCAPE | K_F6) => self.toggle_tree(),
            Key::Sym(K_F7) => self.act(Action::Toggle(crate::Flag::SizeMap), 0),
            Key::Sym(K_UP) => tree.sel = tree.sel.saturating_sub(1),
            Key::Sym(K_DOWN) => tree.sel = (tree.sel + 1).min(last),
            Key::Sym(K_PAGE_UP) => tree.sel = tree.sel.saturating_sub(page),
            Key::Sym(K_PAGE_DOWN) => tree.sel = (tree.sel + page).min(last),
            Key::Sym(K_HOME) => tree.sel = 0,
            Key::Sym(K_END) => tree.sel = last,
            // Right opens a folder, then steps into it; Left does the reverse.
            Key::Sym(K_RIGHT) => {
                if !tree.open(tree.sel) && tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.sel += 1;
                }
            }
            Key::Sym(K_LEFT) => {
                if tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.close(tree.sel);
                } else if let Some(parent) = tree.parent_row(tree.sel) {
                    tree.sel = parent;
                }
            }
            Key::Sym(K_BACKSPACE) => tree.sel = tree.parent_row(tree.sel).unwrap_or(tree.sel),
            Key::Sym(K_RETURN | K_KP_ENTER) if mods.ctrl => self.act(Action::Reveal, 0),
            Key::Sym(K_RETURN | K_KP_ENTER) if mods.alt => self.act(Action::Properties, 0),
            Key::Sym(K_RETURN | K_KP_ENTER) => {
                let sel = tree.sel;
                if tree.rows.get(sel).is_some_and(|n| n.open) {
                    tree.close(sel);
                } else if !tree.open(sel) {
                    self.act(Action::Open, 0);
                }
            }
            Key::Sym(K_DELETE) => self.act(if mods.shift { Action::DeleteForever } else { Action::Trash }, 0),
            Key::Sym(K_MENU) => {
                let row = tree.sel.saturating_sub(tree.top) as i32;
                let (x, y) = (40 * self.u, self.list_top() + (row + 1) * self.row_h());
                let items = self.on_tree_target(|app| app.context_items())?;
                self.menu = Some(Menu { x, y, hover: Some(0), items, bar: None });
            }
            // Typing goes back to the search.
            Key::Char(ch) if !mods.ctrl && !mods.alt && !ch.is_control() => {
                self.toggle_tree();
                return None;
            }
            _ => return None,
        }
        self.status.clear();
        self.show_tree_sel();
        Some(Ok(true))
    }

    /// A button press below the menu bar while the view is open.
    pub(crate) fn tree_press(&mut self, button: u8, x: i32, y: i32, time: u32) {
        let (split, bar, list_top) = (self.tree_split(), self.bar_w(), self.list_top());
        let max_top = self.max_top();
        let Some(tree) = self.tree.as_mut() else { return };
        match button {
            4 => return tree.top = tree.top.saturating_sub(3),
            5 => return tree.top = (tree.top + 3).min(max_top),
            1 | 3 => {}
            _ => return,
        }
        if button == 1 && x >= split - bar && x < split && y >= list_top {
            self.dragging_scrollbar = true;
            return self.scroll_to(y);
        }
        self.status.clear();
        let double = |app: &mut App, id: u32| {
            let twice = app.last_click.1 == id as usize && time.wrapping_sub(app.last_click.0) < 400;
            app.last_click = (time, id as usize);
            twice && button == 1
        };
        if let Some(row) = self.tree_row_at(x, y) {
            let Some(tree) = self.tree.as_mut() else { return };
            let node = &tree.rows[row];
            let (id, open) = (node.id, node.open);
            // The arrow in front of a folder opens and closes it.
            let arrow_x = 4 * self.u + node.depth as i32 * 14 * self.u;
            let on_arrow = button == 1 && x >= arrow_x && x < arrow_x + 16 * self.u;
            tree.sel = row;
            if double(self, id) || on_arrow {
                let tree = self.tree.as_mut().unwrap();
                if open {
                    tree.close(row);
                } else if !tree.open(row) && !on_arrow {
                    self.act(Action::Open, time);
                }
            }
        } else if let Some(tile) = self.tile_at(x, y) {
            // A click in the map goes into that folder.
            double(self, tile.id);
            self.select_in_tree(tile.id);
            if let Some(tree) = self.tree.as_mut() {
                tree.open(tree.sel);
            }
        } else {
            return;
        }
        self.show_tree_sel();
        if button == 3 {
            if let Some(items) = self.on_tree_target(|app| app.context_items()) {
                self.menu = Some(Menu { x, y, hover: None, items, bar: None });
            }
        }
    }

    /// The pointer moved; true if the tile under it changed.
    pub(crate) fn tree_motion(&mut self, x: i32, y: i32) -> bool {
        let hover = self.tile_at(x, y).map(|t| t.id).filter(|_| self.menu.is_none());
        std::mem::replace(&mut self.tile_hover, hover) != hover
    }

    /// The status bar text while the view is open.
    pub(crate) fn tree_status(&self) -> String {
        let Some(tree) = &self.tree else { return String::new() };
        let Some(id) = self.tile_hover.or(tree.selected()) else { return tr("The index is empty").to_string() };
        let size = format!("{} ({})", units(tree.total(id)), trf("{} bytes", &[&group_digits(tree.total(id))]));
        let what = if self.index.is_dir(id) { format!("{size}      {}", trf("{} files", &[&group_digits(tree.files(id) as u64)])) } else { size };
        format!("{}      {what}      {}", self.index.path(id).display(), tr("Esc: back to the search"))
    }

    /// Draws the view below the menu bar: the tree on the left, the map
    /// of the selected folder on the right.
    pub(crate) fn draw_tree(&mut self) {
        let (w, h) = (self.cv.w as i32, self.cv.h as i32);
        let (u, row_h, bar) = (self.u, self.row_h(), self.bar_w());
        let pad = 8 * u;
        let Some(tree) = self.tree.take() else { return };
        let split = self.tree_split();
        let cols = self.tree_columns();

        // Where the search box was: the path of the selected entry. It
        // cannot be edited, so it does not look like the search box.
        let top = self.menu_h();
        let path = tree.selected().map(|id| self.index.path(id).display().to_string()).unwrap_or_default();
        self.cv.round_frame(6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u, 4 * u, u.max(1), LINE, PANEL);
        let path_x = self.box_text_x();
        self.text.draw(&mut self.cv, path_x, top + 10 * u, &path, w - 28 * u, DIM);

        // Column headers, quiet, as in the results list.
        let y = self.box_h();
        self.cv.fill(0, y, split, h - y - row_h, palette::CARD);
        self.cv.fill(pad, y + row_h, split - 2 * pad, 1, LINE);
        let heads = ["Name", "Size", "Files", "Folders", "% of parent", "% of disk", "Free", "Modified"];
        for (i, (label, (x, cw))) in heads.into_iter().zip(cols).enumerate().filter(|(_, (_, (_, cw)))| *cw > 0) {
            if i > 0 {
                self.cv.fill(x, y + 6 * u, 1, row_h - 12 * u, LINE);
            }
            // Numbers are right-aligned, and so are their headings.
            let lx = if (1..4).contains(&i) || i == 6 { x + cw - pad - self.text.width(tr(label)) } else { x + pad };
            self.text.draw(&mut self.cv, lx, y + 3 * u, tr(label), cw - 2 * pad, DIM);
        }

        // The tree.
        let rows = self.rows_visible();
        let list_top = self.list_top();
        let icon = self.icons.size as i32;
        let right = split - bar;
        self.cv.clip = (0, right);
        for row in 0..rows {
            let Some(node) = tree.rows.get(tree.top + row) else { break };
            let (id, y) = (node.id, list_top + row as i32 * row_h);
            let selected = tree.top + row == tree.sel;
            let (fg, dim) = if selected { (ON_ACCENT, ON_ACCENT) } else { (TEXT, DIM) };
            // Its share of the folder it is in (a drive is all of itself).
            let of = self.index.parent(id).map_or(tree.total(id), |p| tree.total(p));
            let share = if of == 0 { 0.0 } else { tree.total(id) as f64 / of as f64 };
            // Its share of its drive. For a whole drive (a root such as C:\
            // or /), how full it is, as the system counts it.
            let whole_drive = self.index.parent(id).is_none() && self.index.path(id).parent().is_none();
            let space = self.disk_of(id);
            let disk = space.map(|(capacity, free)| {
                let used = if whole_drive { capacity.saturating_sub(free) } else { tree.total(id) };
                (used as f64 / capacity as f64).min(1.0)
            });
            let x = pad / 2 + node.depth as i32 * 14 * u;
            if selected {
                self.cv.fill(0, y, right, row_h, SELECT);
            }
            // Behind the name, a quiet bar as long as its share.
            let name_left = x + 16 * u;
            if !selected {
                let room = (cols[0].1 - name_left - pad).max(0);
                self.cv.fill(name_left, y + 3 * u, (room as f64 * share) as i32, row_h - 6 * u, palette::PICKED);
            }
            let is_dir = self.index.is_dir(id);
            self.cv.clip = (0, cols[0].1);
            if !tree.kids(id).is_empty() {
                self.draw_arrow(x + 5 * u, y + row_h / 2, node.open, dim);
            }
            let ((entry_path, drive), system) = (self.entry_path(id), self.flag(crate::Flag::SystemIcons));
            let entry_name = self.index.name(id);
            self.icons.draw_entry(&mut self.cv, name_left, y + (row_h - icon) / 2, entry_name, is_dir, self.index.is_symlink(id), &entry_path, drive, system);
            let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
            let (name_x, ty) = (name_left + icon + 6 * u, y + 3 * u);
            self.text.draw(&mut self.cv, name_x, ty, &name, cols[0].1 - pad - name_x, fg);
            self.cv.clip = (0, right);
            // Size, files and folders, right-aligned; counts kept short.
            let count = |n: u32| if is_dir { ff_core::sizes::short_count(n) } else { String::new() };
            let cells = [(units(tree.total(id)), fg), (count(tree.files(id)), dim), (count(tree.folders(id)), dim)];
            for ((cell, color), &(cx, cw)) in cells.into_iter().zip(&cols[1..4]) {
                if cw > 0 {
                    let cell_w = self.text.width(&cell);
                    self.text.draw(&mut self.cv, cx + cw - pad - cell_w, ty, &cell, cw, color);
                }
            }
            self.draw_share(cols[4], y, Some(share), SELECT, selected);
            if cols[5].1 > 0 {
                // As Explorer draws drives: blue, red once 90 % full.
                let color = if disk.is_some_and(|d| d >= 0.9) { DISK_FULL } else { DISK_OK };
                self.draw_share(cols[5], y, disk, color, selected);
            }
            self.cv.clip = (0, right);
            // The space left on a whole drive (the same for all inside it,
            // so shown on the drive only).
            let (fx, fw) = cols[6];
            if let (true, true, Some((_, free))) = (fw > 0, whole_drive, space) {
                let cell = units(free);
                let cell_w = self.text.width(&cell);
                self.text.draw(&mut self.cv, fx + fw - pad - cell_w, ty, &cell, fw, fg);
            }
            // When it last changed: for a folder, its newest content.
            let (dx, dw) = cols[7];
            if dw > 0 {
                let (yr, mo, d, ..) = ff_core::local_civil(tree.newest(id));
                self.text.draw(&mut self.cv, dx + pad, ty, &format!("{yr:04}-{mo:02}-{d:02}"), dw - pad, dim);
            }
        }
        self.cv.clip = (0, i32::MAX);
        if tree.rows.len() > rows {
            let (track, thumb) = self.scrollbar();
            let pos = ((track - thumb) as i64 * tree.top as i64 / (tree.rows.len() - rows).max(1) as i64) as i32;
            let tw = 4 * u;
            self.cv.round(right + (bar - tw) / 2, list_top + pos + 2 * u, tw, thumb - 4 * u, tw / 2, palette::QUIET);
        }

        // The map.
        self.tiles.clear();
        let (mx, my) = (split + 1, self.box_h());
        let (mw, mh) = (w - mx, h - row_h - my);
        if mw > 0 {
            self.cv.fill(split, my, 1, mh, LINE);
            self.cv.fill(mx, my, mw, mh, WHITE);
        }
        let header = self.text.height + 4 * u;
        if let Some(root) = self.map_root(&tree).filter(|_| mw > 8 * header && mh > 4 * header) {
            let mut tiles = std::mem::take(&mut self.tiles);
            tree.map(root, (mx + 4 * u, my + 4 * u, mw - 8 * u, mh - 8 * u), header, &mut tiles);
            for tile in &tiles {
                self.draw_tile(&tree, tile);
            }
            // Outlines last, so nothing drawn later covers them.
            for tile in &tiles {
                if Some(tile.id) == tree.selected() && tile.depth > 0 {
                    self.cv.frame(tile.x, tile.y, tile.w, tile.h, 2 * u, SELECT);
                } else if Some(tile.id) == self.tile_hover {
                    self.cv.frame(tile.x, tile.y, tile.w, tile.h, u.max(1), 0x333333);
                }
            }
            if tiles.len() == 1 {
                let note = tr(if tree.kids(root).is_empty() { "This folder is empty" } else { "Everything in this folder is empty" });
                self.text.draw(&mut self.cv, mx + 12 * u, my + 4 * u + header + 8 * u, note, mw - 24 * u, DIM);
            }
            self.tiles = tiles;
        }
        self.cv.clip = (0, i32::MAX);
        self.tree = Some(tree);
    }

    fn draw_tile(&mut self, tree: &Tree, tile: &Tile) {
        let u = self.u;
        let Tile { id, x, y, w, h, .. } = *tile;
        // Folders get deeper blues level by level; files are sand.
        let color = if self.index.is_dir(id) { [0xdce9fb, 0xcfe0f9, 0xc6d9f6, 0xc1d4f2][tile.depth.min(3) as usize] } else { 0xfbefd0 };
        self.cv.clip = (x, x + w);
        self.cv.fill(x, y, w, h, color);
        self.cv.frame(x, y, w, h, 1, WHITE);
        let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
        let size = units(tree.total(id));
        self.cv.clip = (x, x + w - 2 * u);
        if tile.framed {
            self.text.draw(&mut self.cv, x + 4 * u, y + 2 * u, &format!("{name}  ({size})"), w - 6 * u, TEXT);
        } else if w > 44 * u && h >= self.text.height + 2 * u {
            self.text.draw(&mut self.cv, x + 4 * u, y + u, &name, w - 6 * u, TEXT);
            if h >= 2 * self.text.height + 3 * u {
                self.text.draw(&mut self.cv, x + 4 * u, y + u + self.text.height, &size, w - 6 * u, DIM);
            }
        }
    }

    /// A small triangle: pointing down for an open folder, right for a
    /// closed one. (`x`, `y`) is its centre.
    /// A share as a bar in column `(x, w)` of the row at `y`, with its
    /// figure on it: white over the filled part, ink over the rest.
    /// Nothing is drawn for an unknown share.
    fn draw_share(&mut self, (x, w): (i32, i32), y: i32, share: Option<f64>, color: u32, selected: bool) {
        let Some(share) = share.filter(|_| w > 0) else { return };
        let (u, row_h) = (self.u, self.row_h());
        let pad = 8 * u;
        let (bx, bw, bh, ty) = (x + pad / 2, w - pad, row_h - 6 * u, y + 3 * u);
        let filled = (bw as f64 * share).round() as i32;
        let saved = self.cv.clip;
        // On the selected (accent-filled) row the bar is inverted: white
        // fill on an outline of white, so it still shows.
        let (track, fill, ink_fill, ink_track) =
            if selected { (SELECT, ON_ACCENT, SELECT, ON_ACCENT) } else { (LINE, color, ON_ACCENT, TEXT) };
        if selected {
            self.cv.round_frame(bx, ty, bw, bh, 3 * u, u.max(1), ON_ACCENT, track);
        } else {
            self.cv.round(bx, ty, bw, bh, 3 * u, track);
        }
        if filled > 0 {
            self.cv.clip = (bx, bx + filled);
            self.cv.round(bx, ty, bw, bh, 3 * u, fill);
        }
        let figure = format!("{:.1} %", share * 100.0);
        let fx = bx + (bw - self.text.width(&figure)) / 2;
        self.cv.clip = (bx, bx + filled);
        self.text.draw(&mut self.cv, fx, ty, &figure, bw, ink_fill);
        self.cv.clip = (bx + filled, bx + bw);
        self.text.draw(&mut self.cv, fx, ty, &figure, bw, ink_track);
        self.cv.clip = saved;
    }

    fn draw_arrow(&mut self, x: i32, y: i32, open: bool, color: u32) {
        let s = 4 * self.u;
        for i in 0..s {
            if open {
                self.cv.fill(x - s + i, y - s / 2 + i, 2 * (s - i), 1, color);
            } else {
                self.cv.fill(x - s / 2 + i, y - s + i, 1, 2 * (s - i), color);
            }
        }
    }
}
