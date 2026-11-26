//! Things kept for later: bookmarks (saved searches) and the results
//! written out as a CSV file.

use std::io::{self, Write};
use std::path::PathBuf;

use ff_core::bookmarks::{self, Bookmark, format};
use ff_core::{Kind, group_digits};

use crate::dialog::Row;
use crate::i18n::{tr, trf};
use crate::{Action, App, Items};

/// The entries of the Bookmarks menu that are always there.
const MENU_HEAD: [(&str, &str, Action); 2] = [("Bookmark this search...", "Ctrl+D", Action::AddBookmark), ("Organize bookmarks...", "", Action::Options(8))];

/// A field of a CSV file: quoted when it holds a comma, a quote or a
/// line break, with quotes doubled.
pub fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
}

impl App {
    /// The Bookmarks menu: its fixed entries, then the bookmarks. Menus
    /// hold fixed text, so this is rebuilt (and the old one left behind)
    /// each time the bookmarks change, which is rare.
    pub(crate) fn rebuild_bookmark_menu(&mut self) {
        let mut items = MENU_HEAD.to_vec();
        for (i, bookmark) in self.bookmarks.iter().enumerate().take(20) {
            let mut name: String = bookmark.name.chars().take(48).collect();
            if name.len() < bookmark.name.len() {
                name.push('…');
            }
            let hint = ["Alt+1", "Alt+2", "Alt+3", "Alt+4", "Alt+5", "Alt+6", "Alt+7", "Alt+8", "Alt+9"].get(i).copied().unwrap_or("");
            items.push((Box::leak(name.into_boxed_str()), hint, Action::Bookmark(i as u8)));
        }
        self.bookmark_items = Box::leak(items.into_boxed_slice());
    }

    pub(crate) fn bookmark_menu(&self) -> Items {
        self.bookmark_items
    }

    fn save_bookmarks(&mut self) {
        let saved = bookmarks::file().and_then(|path| {
            std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")))?;
            std::fs::write(path, format(&self.bookmarks))
        });
        if let Err(e) = saved {
            self.status = format!("could not save the bookmarks: {e}");
        }
        self.rebuild_bookmark_menu();
    }

    /// Asks for a name for the current search.
    pub(crate) fn ask_bookmark(&mut self) {
        if self.query.trim().is_empty() {
            self.status = tr("Type a search first, then bookmark it").into();
            return;
        }
        let rows = vec![Row::Text(tr("Name of the bookmark:").into()), Row::Input("Name", Action::SaveBookmark), Row::Gap, Row::Pair("Search", self.query.clone())];
        let buttons = vec![("Cancel", None), ("Save", Some(Action::SaveBookmark))];
        let name = self.query.trim().to_string();
        self.open_input_dialog("Bookmark this search", rows, buttons, name);
    }

    /// Saves the current search under the name typed in the dialog.
    pub(crate) fn save_bookmark(&mut self) {
        let typed = std::mem::take(&mut self.dialog_input);
        self.close_dialog();
        let name = typed.replace('\t', " ").trim().to_string();
        let name = if name.is_empty() { self.query.trim().to_string() } else { name };
        if self.query.trim().is_empty() || name.is_empty() {
            return;
        }
        let mut flags = String::new();
        for (on, letter) in [(self.regex, 'r'), (self.whole_word, 'w'), (self.kind == Kind::File, 'f'), (self.kind == Kind::Dir, 'd')] {
            if on {
                flags.push(letter);
            }
        }
        let bookmark = Bookmark { name: name.clone(), query: self.query.replace('\t', " "), flags };
        // The same name again replaces that bookmark.
        match self.bookmarks.iter_mut().find(|b| b.name == name) {
            Some(old) => *old = bookmark,
            None => self.bookmarks.push(bookmark),
        }
        self.status = trf("Bookmarked as \"{}\"", &[&name]);
        self.save_bookmarks();
    }

    /// Runs a bookmark: its search, read the way it was when saved.
    pub(crate) fn open_bookmark(&mut self, i: usize) {
        let Some(bookmark) = self.bookmarks.get(i).cloned() else {
            self.status = trf("There is no bookmark {}", &[&(i + 1)]);
            return;
        };
        if self.tree.is_some() {
            self.toggle_tree();
        }
        self.complete = None;
        self.regex = bookmark.flags.contains('r');
        self.whole_word = bookmark.flags.contains('w');
        self.kind = match (bookmark.flags.contains('f'), bookmark.flags.contains('d')) {
            (true, _) => Kind::File,
            (_, true) => Kind::Dir,
            _ => Kind::Any,
        };
        self.query = bookmark.query;
        (self.cursor, self.anchor) = (self.query.len(), self.query.len());
        self.search();
        self.status.clear();
    }

    pub(crate) fn remove_bookmark(&mut self) {
        if self.bookmark_selected < self.bookmarks.len() {
            self.bookmarks.remove(self.bookmark_selected);
            self.bookmark_selected = self.bookmark_selected.min(self.bookmarks.len().saturating_sub(1));
            self.save_bookmarks();
        }
    }

    /// Moves the selected bookmark one place up the list (and the menu).
    pub(crate) fn raise_bookmark(&mut self) {
        let i = self.bookmark_selected;
        if i > 0 && i < self.bookmarks.len() {
            self.bookmarks.swap(i, i - 1);
            self.bookmark_selected = i - 1;
            self.save_bookmarks();
        }
    }

    /// The Bookmarks page of the Options window.
    pub(crate) fn bookmark_rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Dim("Saved searches:".into())];
        if self.bookmarks.is_empty() {
            rows.push(Row::Dim("      (none)".into()));
        }
        for (i, bookmark) in self.bookmarks.iter().enumerate().take(12) {
            let shown = if bookmark.name == bookmark.query.trim() { bookmark.name.clone() } else { format!("{}      {}", bookmark.name, bookmark.query) };
            rows.push(Row::ListItem(shown, Action::SelectBookmark(i as u8)));
        }
        if !self.bookmarks.is_empty() {
            rows.push(Row::Button("Move the selected bookmark up", Action::RaiseBookmark));
            rows.push(Row::Button("Remove the selected bookmark", Action::RemoveBookmark));
        }
        rows.push(Row::Gap);
        rows.push(Row::Dim("Ctrl+D bookmarks the current search. The Bookmarks menu opens them;".into()));
        rows.push(Row::Dim("Alt+1 to Alt+9 open the first nine.".into()));
        rows
    }

    // ---- export ---------------------------------------------------------

    /// How many rows an export would write: the selected results when
    /// several are selected, else all of them; in the folder sizes view,
    /// the rows of the tree.
    fn export_count(&self) -> usize {
        match &self.tree {
            Some(tree) => tree.rows.len(),
            None if self.marks.len() > 1 => self.marks.len(),
            None => self.hits.len(),
        }
    }

    /// Asks where to save the results.
    pub(crate) fn ask_export(&mut self) {
        let n = self.export_count();
        if n == 0 {
            self.status = tr("There are no results to export").into();
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let (y, mo, d, h, mi) = ff_core::local_civil(now);
        let home = ff_core::os::home_dir().unwrap_or_default();
        let folder = Some(home.join("Downloads")).filter(|d| d.is_dir()).unwrap_or(home);
        let path = folder.join(format!("fast-search-{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}.csv"));
        let rows = vec![Row::Text(trf("Save {} rows as a CSV file (for a spreadsheet):", &[&group_digits(n as u64)])), Row::Input("/path/to/file.csv", Action::ConfirmExport)];
        let buttons = vec![("Cancel", None), ("Export", Some(Action::ConfirmExport))];
        self.open_input_dialog("Export results", rows, buttons, path.to_string_lossy().into_owned());
    }

    /// Writes the results to the file named in the dialog.
    pub(crate) fn export(&mut self) {
        let typed = std::mem::take(&mut self.dialog_input);
        self.close_dialog();
        let typed = typed.trim();
        let path = match typed.strip_prefix("~/").or_else(|| typed.strip_prefix("~\\")) {
            Some(rest) => ff_core::os::home_dir().unwrap_or_default().join(rest),
            None => PathBuf::from(typed),
        };
        if !path.is_absolute() {
            self.status = tr("The file to export to must be a full path, like /home/me/results.csv").into();
            return;
        }
        self.status = match self.write_csv(&path) {
            Ok(n) => trf("Saved {} rows to {}", &[&group_digits(n as u64), &path.display()]),
            Err(e) => format!("{}: {e}", path.display()),
        };
    }

    fn write_csv(&self, path: &std::path::Path) -> io::Result<usize> {
        let mut out = io::BufWriter::new(std::fs::File::create(path)?);
        let text = |bytes: &[u8]| csv_field(&String::from_utf8_lossy(bytes));
        let mut folder = Vec::new();
        let mut location = |id: u32| match self.index.parent(id) {
            Some(parent) => {
                self.index.write_path(parent, &mut folder);
                text(&folder)
            }
            None => String::new(),
        };
        let date = |id: u32| {
            let (y, mo, d, h, mi) = ff_core::local_civil(self.index.mtime(id));
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
        };
        let kind = |id: u32| if self.index.is_dir(id) { "folder" } else { "file" };
        if let Some(tree) = &self.tree {
            writeln!(out, "Name,Folder,Type,Size (bytes),Files,Level")?;
            for node in &tree.rows {
                let id = node.id;
                writeln!(out, "{},{},{},{},{},{}", text(self.index.name(id)), location(id), kind(id), tree.total(id), tree.files(id), node.depth)?;
            }
            out.flush()?;
            return Ok(tree.rows.len());
        }
        let rows: Vec<usize> = if self.marks.len() > 1 { self.selected_rows() } else { (0..self.hits.len()).collect() };
        writeln!(out, "Name,Folder,Type,Size (bytes),Modified")?;
        for &row in &rows {
            let id = self.hits[row];
            // A folder's own size says nothing useful: left empty.
            let size = if self.index.is_dir(id) { String::new() } else { self.index.size(id).to_string() };
            writeln!(out, "{},{},{},{size},{}", text(self.index.name(id)), location(id), kind(id), date(id))?;
        }
        out.flush()?;
        Ok(rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_when_needed() {
        assert_eq!(csv_field("report.pdf"), "report.pdf");
        assert_eq!(csv_field("a, b.txt"), "\"a, b.txt\"");
        assert_eq!(csv_field("say \"hi\".txt"), "\"say \"\"hi\"\".txt\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
    }
}
