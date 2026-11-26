//! Bookmarks: searches saved under a name, kept in a small text file
//! that the window and the terminal interface both read.

use std::io;
use std::path::PathBuf;

/// A saved search: what was typed and how it was read.
#[derive(Clone, PartialEq, Debug)]
pub struct Bookmark {
    pub name: String,
    pub query: String,
    /// Letters: r regular expressions, w whole words, f files only,
    /// d folders only.
    pub flags: String,
}

/// `~/.config/ff/bookmarks.tsv` (or under `$XDG_CONFIG_HOME`; on
/// Windows `%APPDATA%\ff\bookmarks.tsv`).
pub fn file() -> io::Result<PathBuf> {
    let config = crate::os::config_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(config.join("ff").join("bookmarks.tsv"))
}

/// One bookmark per line: name, search and flags, separated by tabs.
pub fn parse(text: &str) -> Vec<Bookmark> {
    let line = |l: &str| {
        let mut parts = l.split('\t');
        let (name, query) = (parts.next()?.trim(), parts.next()?);
        (!name.is_empty() && !query.trim().is_empty()).then(|| Bookmark { name: name.into(), query: query.into(), flags: parts.next().unwrap_or("").trim().into() })
    };
    text.lines().filter_map(line).collect()
}

pub fn format(bookmarks: &[Bookmark]) -> String {
    bookmarks.iter().map(|b| format!("{}\t{}\t{}\n", b.name, b.query, b.flags)).collect()
}

pub fn load() -> Vec<Bookmark> {
    file().and_then(std::fs::read_to_string).map(|text| parse(&text)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_survive_saving() {
        let bookmarks = vec![
            Bookmark { name: "Big recent files".into(), query: "size:>50M modified:30d".into(), flags: "f".into() },
            Bookmark { name: "ext:pdf invoice".into(), query: "ext:pdf invoice".into(), flags: String::new() },
        ];
        assert_eq!(parse(&format(&bookmarks)), bookmarks);
        // Damaged lines are skipped, not fatal.
        assert_eq!(parse("only a name\n\t\t\nok\tquery\n").len(), 1);
    }
}
