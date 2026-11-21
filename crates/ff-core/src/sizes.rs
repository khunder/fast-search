//! Folder sizes: what every folder holds in total, as a tree that opens
//! level by level, and the layout of a map in which each tile's area is
//! its size. All of it comes from the index; nothing on disk is read.

use crate::Index;

/// The deepest nesting drawn inside the map.
const MAP_DEPTH: u8 = 3;
/// More tiles than this are too small to tell apart anyway.
const MAX_TILES: usize = 4000;

/// A visible line of the tree.
pub struct Node {
    pub id: u32,
    pub depth: u16,
    pub open: bool,
}

/// A rectangle of the map.
#[derive(Clone, Copy)]
pub struct Tile {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub depth: u8,
    /// Its contents are drawn inside it, under a title.
    pub framed: bool,
}

pub struct Tree {
    /// Per entry: a file's size, or everything inside a folder.
    total: Vec<u64>,
    /// Per entry: how many files it holds (1 for a file).
    files: Vec<u32>,
    /// The children of entry `i` are `kids[first[i]..first[i + 1]]`,
    /// largest first.
    first: Vec<u32>,
    kids: Vec<u32>,
    roots: Vec<u32>,
    pub rows: Vec<Node>,
    pub sel: usize,
    pub top: usize,
}

/// What to bring back after the index changed: the open folders and
/// the selected entry, as paths.
pub struct Kept {
    open: Vec<Vec<u8>>,
    sel: Option<Vec<u8>>,
    top: usize,
}

impl Tree {
    pub fn new(index: &Index) -> Tree {
        let n = index.len();
        let mut total = vec![0u64; n];
        let mut files = vec![0u32; n];
        let mut first = vec![0u32; n + 2];
        for id in 0..n as u32 {
            if let Some(p) = index.parent(id) {
                first[p as usize + 2] += 1;
            }
            if index.is_dir(id) {
                continue;
            }
            // A file counts for every folder above it.
            let size = index.size(id);
            (total[id as usize], files[id as usize]) = (size, 1);
            let mut up = index.parent(id);
            while let Some(p) = up {
                total[p as usize] += size;
                files[p as usize] += 1;
                up = index.parent(p);
            }
        }
        for i in 2..n + 2 {
            first[i] += first[i - 1];
        }
        let mut kids = vec![0u32; first[n + 1] as usize];
        let mut roots = Vec::new();
        for id in 0..n as u32 {
            match index.parent(id) {
                Some(p) => {
                    let slot = &mut first[p as usize + 1];
                    kids[*slot as usize] = id;
                    *slot += 1;
                }
                None => roots.push(id),
            }
        }
        first.pop();
        let largest_first = |list: &mut [u32]| list.sort_unstable_by(|&a, &b| total[b as usize].cmp(&total[a as usize]).then(a.cmp(&b)));
        for i in 0..n {
            largest_first(&mut kids[first[i] as usize..first[i + 1] as usize]);
        }
        largest_first(&mut roots);
        let rows = roots.iter().map(|&id| Node { id, depth: 0, open: false }).collect();
        let mut tree = Tree { total, files, first, kids, roots, rows, sel: 0, top: 0 };
        if tree.roots.len() == 1 {
            tree.open(0);
        }
        tree
    }

    pub fn total(&self, id: u32) -> u64 {
        self.total[id as usize]
    }

    pub fn files(&self, id: u32) -> u32 {
        self.files[id as usize]
    }

    pub fn kids(&self, id: u32) -> &[u32] {
        &self.kids[self.first[id as usize] as usize..self.first[id as usize + 1] as usize]
    }

    pub fn selected(&self) -> Option<u32> {
        self.rows.get(self.sel).map(|n| n.id)
    }

    /// Shows the children of a row; returns false if it has none.
    pub fn open(&mut self, row: usize) -> bool {
        let Some(node) = self.rows.get(row).filter(|n| !n.open) else { return false };
        let (id, depth) = (node.id, node.depth + 1);
        let range = self.first[id as usize] as usize..self.first[id as usize + 1] as usize;
        if range.is_empty() {
            return false;
        }
        let children: Vec<Node> = self.kids[range].iter().map(|&id| Node { id, depth, open: false }).collect();
        if self.sel > row {
            self.sel += children.len();
        }
        self.rows[row].open = true;
        self.rows.splice(row + 1..row + 1, children);
        true
    }

    pub fn close(&mut self, row: usize) {
        let Some(node) = self.rows.get_mut(row).filter(|n| n.open) else { return };
        node.open = false;
        let depth = node.depth;
        let end = self.rows[row + 1..].iter().position(|n| n.depth <= depth).map_or(self.rows.len(), |i| row + 1 + i);
        // A selection inside the closed folder moves to the folder.
        if self.sel >= end {
            self.sel -= end - row - 1;
        } else if self.sel > row {
            self.sel = row;
        }
        self.rows.drain(row + 1..end);
    }

    pub fn parent_row(&self, row: usize) -> Option<usize> {
        let depth = self.rows.get(row)?.depth;
        self.rows[..row].iter().rposition(|n| n.depth < depth)
    }

    /// Opens the folders leading to `id` and selects it.
    pub fn reveal(&mut self, index: &Index, id: u32) -> bool {
        let mut chain = vec![id];
        while let Some(p) = index.parent(*chain.last().unwrap()) {
            chain.push(p);
        }
        let root = chain.pop().unwrap();
        let Some(mut row) = self.rows.iter().position(|n| n.depth == 0 && n.id == root) else { return false };
        while let Some(next) = chain.pop() {
            self.open(row);
            let depth = self.rows[row].depth;
            let inside = self.rows[row + 1..].iter().take_while(|n| n.depth > depth);
            let Some(i) = inside.into_iter().position(|n| n.depth == depth + 1 && n.id == next) else { return false };
            row += 1 + i;
        }
        self.sel = row;
        true
    }

    /// The entry with that full path, if it is in the index.
    fn find(&self, index: &Index, path: &[u8]) -> Option<u32> {
        let sep = crate::os::is_sep;
        let inside = |root: &[u8]| path.strip_prefix(root).filter(|rest| rest.is_empty() || sep(rest[0]) || root.last().is_some_and(|&b| sep(b)));
        let root = self.roots.iter().copied().filter(|&r| inside(index.name(r)).is_some()).max_by_key(|&r| index.name(r).len())?;
        let rest = inside(index.name(root))?;
        rest.split(|&b| sep(b)).filter(|part| !part.is_empty()).try_fold(root, |dir, part| self.kids(dir).iter().copied().find(|&k| index.name(k) == part))
    }

    pub fn keep(&self, index: &Index) -> Kept {
        let path = |id: u32| {
            let mut buf = Vec::new();
            index.write_path(id, &mut buf);
            buf
        };
        Kept { open: self.rows.iter().filter(|n| n.open).map(|n| path(n.id)).collect(), sel: self.selected().map(path), top: self.top }
    }

    /// The tree of a new index, opened like the old one was.
    pub fn restored(index: &Index, kept: &Kept) -> Tree {
        let mut tree = Tree::new(index);
        for path in &kept.open {
            if let Some(id) = tree.find(index, path) {
                if tree.reveal(index, id) {
                    tree.open(tree.sel);
                }
            }
        }
        tree.sel = 0;
        // What was selected may be gone: then the nearest folder still there.
        let mut path = kept.sel.as_deref();
        while let Some(p) = path {
            if tree.find(index, p).is_some_and(|id| tree.reveal(index, id)) {
                break;
            }
            // The parent; for `C:\x` that is the root `C:\`, kept whole.
            path = p.iter().rposition(|&b| crate::os::is_sep(b)).filter(|&i| i > 0).map(|i| {
                if p[..i].iter().any(|&b| crate::os::is_sep(b)) { &p[..i] } else { &p[..=i] }
            });
            if path == Some(p) {
                break;
            }
        }
        tree.top = kept.top;
        tree
    }

    /// Lays the contents of folder `id` out in a rectangle, each tile's
    /// area in proportion to its size; large folders show their own
    /// contents under a title `header` pixels high.
    pub fn map(&self, id: u32, rect: (i32, i32, i32, i32), header: i32, out: &mut Vec<Tile>) {
        let (x, y, w, h) = rect;
        out.push(Tile { id, x, y, w, h, depth: 0, framed: true });
        self.lay(id, (x + 2, y + header, w - 4, h - header - 2), 1, header, out);
    }

    fn lay(&self, id: u32, rect: (i32, i32, i32, i32), depth: u8, header: i32, out: &mut Vec<Tile>) {
        let (x, y, w, h) = rect;
        let sum = self.total(id);
        if sum == 0 || w < 4 || h < 4 {
            return;
        }
        // Children are sorted, so the ones too small to draw are the tail.
        let scale = w as f64 * h as f64 / sum as f64;
        let room = MAX_TILES.saturating_sub(out.len());
        let areas: Vec<f64> = self.kids(id).iter().map(|&k| self.total(k) as f64 * scale).take_while(|&a| a >= 12.0).take(room).collect();
        let rects = squarify(&areas, x as f64, y as f64, w as f64, h as f64);
        for (&kid, (rx, ry, rw, rh)) in self.kids(id).iter().zip(rects) {
            // Rounding both edges keeps neighbours flush.
            let (x0, y0) = (rx.round() as i32, ry.round() as i32);
            let (tw, th) = ((rx + rw).round() as i32 - x0, (ry + rh).round() as i32 - y0);
            if tw < 1 || th < 1 {
                continue;
            }
            let framed = depth < MAP_DEPTH && !self.kids(kid).is_empty() && tw > 4 * header && th > 3 * header;
            out.push(Tile { id: kid, x: x0, y: y0, w: tw, h: th, depth, framed });
            if framed {
                self.lay(kid, (x0 + 2, y0 + header, tw - 4, th - header - 2), depth + 1, header, out);
            }
        }
    }
}

/// The "squarified" layout: fills the rectangle strip by strip along its
/// short side, closing a strip when one more tile would make its tiles
/// less square. `areas` are in square pixels, largest first.
fn squarify(areas: &[f64], mut x: f64, mut y: f64, mut w: f64, mut h: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::with_capacity(areas.len());
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h);
        if side <= 0.0 {
            break;
        }
        // The worst width-to-height ratio in a strip of that total.
        let worst = |sum: f64, small: f64, large: f64| (side * side * large / (sum * sum)).max(sum * sum / (side * side * small));
        let (mut j, mut sum) = (i + 1, areas[i]);
        let mut ratio = worst(sum, areas[i], areas[i]);
        while j < areas.len() {
            let with = worst(sum + areas[j], areas[j], areas[i]);
            if with > ratio {
                break;
            }
            (sum, ratio, j) = (sum + areas[j], with, j + 1);
        }
        let thick = sum / side;
        let mut along = 0.0;
        for &area in &areas[i..j] {
            let len = area / thick;
            out.push(if w >= h { (x, y + along, thick, len) } else { (x + along, y, len, thick) });
            along += len;
        }
        if w >= h {
            (x, w) = (x + thick, w - thick);
        } else {
            (y, h) = (y + thick, h - thick);
        }
        i = j;
    }
    out
}

/// A size in the largest unit that keeps the number short: "512 B",
/// "3.4 MB", "1.2 GB" (units of 1024).
pub fn human_size(bytes: u64) -> String {
    let (mut value, mut unit) = (bytes as f64, "B");
    for next in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_tree_and_map() {
        let root = std::env::temp_dir().join(format!("ff_tree_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("big/deep")).unwrap();
        std::fs::create_dir_all(root.join("small")).unwrap();
        std::fs::write(root.join("big/deep/a.bin"), vec![0; 30_000]).unwrap();
        std::fs::write(root.join("big/b.bin"), vec![0; 10_000]).unwrap();
        std::fs::write(root.join("small/c.bin"), vec![0; 5_000]).unwrap();
        std::fs::write(root.join("d.bin"), vec![0; 20_000]).unwrap();
        let index = Index::build(&crate::BuildOptions::new([&root])).unwrap();
        let name = |id: u32| String::from_utf8_lossy(index.name(id)).into_owned();

        // One root: it starts open, its children largest first.
        let mut tree = Tree::new(&index);
        let top = tree.rows[0].id;
        assert_eq!((tree.total(top), tree.files(top)), (65_000, 4));
        let shown: Vec<_> = tree.rows[1..].iter().map(|n| (name(n.id), tree.total(n.id))).collect();
        assert_eq!(shown, [("big".to_string(), 40_000), ("d.bin".to_string(), 20_000), ("small".to_string(), 5_000)]);

        // Opening, closing and jumping to an entry.
        assert!(tree.open(1));
        assert_eq!(name(tree.rows[2].id), "deep");
        assert_eq!(tree.parent_row(2), Some(1));
        tree.sel = 2;
        tree.close(1);
        assert_eq!((tree.rows.len(), tree.sel), (4, 1));
        let deepest = (0..index.len() as u32).find(|&id| name(id) == "a.bin").unwrap();
        assert!(tree.reveal(&index, deepest));
        assert_eq!(tree.selected(), Some(deepest));
        assert_eq!(tree.rows[tree.sel].depth, 3);

        // The same folders are open after the index is replaced.
        let again = Tree::restored(&index, &tree.keep(&index));
        assert_eq!(again.selected(), Some(deepest));
        assert_eq!(again.rows.len(), tree.rows.len());

        // The map: tiles stay inside their folder and areas follow sizes.
        let mut tiles = Vec::new();
        tree.map(top, (0, 0, 400, 300), 20, &mut tiles);
        let tile = |n: &str| *tiles.iter().find(|t| name(t.id) == n).unwrap();
        let (big, d, a) = (tile("big"), tile("d.bin"), tile("a.bin"));
        assert!(big.framed && a.x >= big.x && a.y >= big.y + 20 && a.x + a.w <= big.x + big.w && a.y + a.h <= big.y + big.h);
        let ratio = (big.w * big.h) as f64 / (d.w * d.h) as f64;
        assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn squarified_tiles_fill_the_rectangle() {
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0].map(|a| a * 100.0);
        let rects = squarify(&areas, 0.0, 0.0, 60.0, 40.0);
        assert_eq!(rects.len(), areas.len());
        for (&area, &(x, y, w, h)) in areas.iter().zip(&rects) {
            assert!((w * h - area).abs() < 1e-6);
            assert!(x >= -1e-6 && y >= -1e-6 && x + w <= 60.0 + 1e-6 && y + h <= 40.0 + 1e-6);
            // The point of the algorithm: no long thin slivers.
            assert!(w.max(h) / w.min(h) < 4.0, "{w} x {h}");
        }
    }
}
