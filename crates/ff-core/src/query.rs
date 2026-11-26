use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::index::Index;

/// How letter case is treated when matching.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-insensitive unless the term contains an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// Restrict results to files or directories.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Any,
    /// Everything that is not a directory.
    File,
    Dir,
}

/// Result ordering.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Index order (fastest).
    #[default]
    None,
    Name,
    Path,
    Size,
    Mtime,
}

#[derive(Clone, Default)]
pub struct SearchOptions {
    pub kind: Kind,
    pub sort: Sort,
    pub reverse: bool,
    /// Maximum number of results; 0 means unlimited.
    pub limit: usize,
    /// Search threads; 0 picks a default from the CPU count.
    pub threads: usize,
}

/// How a text term is compared with a name or path.
enum Pattern {
    /// The text occurs somewhere (bytes, lowercased when case-insensitive).
    Substring(Vec<u8>),
    /// The same, but only as a whole word.
    Word(Vec<u8>),
    /// A wildcard pattern (`*`, `?`) for the whole name or path.
    Glob(Vec<u8>),
    Regex(regex_lite::Regex),
}

struct Term {
    pattern: Pattern,
    ci: bool,
    /// Match against the full path instead of the file name.
    path: bool,
    negate: bool,
}

/// A condition on something other than the name's text.
enum Filter {
    /// `ext:` lowercase extensions without the dot; files only.
    Ext(Vec<Vec<u8>>),
    /// `size:` inclusive byte range; files only.
    Size(u64, u64),
    /// `modified:` Unix seconds, start inclusive, end exclusive.
    Modified(i64, i64),
}

/// How the text of a query is to be read.
#[derive(Clone, Copy, Default)]
pub struct QueryOptions {
    pub case: CaseMode,
    /// Terms are regular expressions instead of plain text and wildcards.
    pub regex: bool,
    /// Plain-text terms only match whole words.
    pub whole_word: bool,
    /// Every term is matched against the full path, not just the name.
    pub match_path: bool,
    /// A wildcard pattern may match any part of the name instead of
    /// having to cover all of it.
    pub wildcards_anywhere: bool,
}

/// A parsed query: every term and filter must match (AND).
///
/// Terms:
/// - `foo`        name contains "foo"
/// - `*.pdf`      wildcard for the whole name (`*` any run, `?` one character)
/// - `src/main`   contains a `/`: matched against the full path
/// - `!foo`       negation
/// - `"my file"`  quotes keep spaces inside a term
///
/// Filters:
/// - `ext:mp4;mkv`           by extension; compound ones like `ext:tar.gz` work
/// - `size:>1G`              also `>=`, `<`, `<=`, exact, and ranges `10M..100M`
/// - `modified:today`        also `yesterday`, `3h`, `7d`, a date
///   `2026-10-24`, and ranges `2026-01-01..2026-10-24` (`dm:` for short)
///
/// Matching ignores case unless a term has an uppercase letter; that
/// covers all Unicode letters, not only ASCII.
#[derive(Default)]
pub struct Query {
    terms: Vec<Term>,
    filters: Vec<Filter>,
    /// What was wrong with the query text, if anything.
    error: Option<String>,
}

impl Query {
    /// Splits `text` on whitespace into terms; double quotes group a term
    /// containing spaces (`"my file"`).
    pub fn parse(text: &str) -> Query {
        Self::parse_opts(text, &QueryOptions::default())
    }

    pub fn parse_with(text: &str, case: CaseMode) -> Query {
        Self::parse_opts(text, &QueryOptions { case, ..Default::default() })
    }

    pub fn parse_opts(text: &str, opts: &QueryOptions) -> Query {
        let mut terms: Vec<Vec<u8>> = Vec::new();
        let mut cur = Vec::new();
        let mut quoted = false;
        for &b in text.as_bytes() {
            match b {
                b'"' => quoted = !quoted,
                b if b.is_ascii_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        terms.push(std::mem::take(&mut cur));
                    }
                }
                b => cur.push(b),
            }
        }
        if !cur.is_empty() {
            terms.push(cur);
        }
        Self::from_terms_opts(terms, opts)
    }

    /// Builds a query from already-split terms (e.g. command-line arguments).
    pub fn from_terms<T: AsRef<[u8]>>(terms: impl IntoIterator<Item = T>, case: CaseMode) -> Query {
        Self::from_terms_opts(terms, &QueryOptions { case, ..Default::default() })
    }

    pub fn from_terms_opts<T: AsRef<[u8]>>(terms: impl IntoIterator<Item = T>, opts: &QueryOptions) -> Query {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut query = Query::default();
        for term in terms {
            let raw = term.as_ref();
            if raw.is_empty() {
                continue;
            }
            match parse_filter(raw, now, crate::time::local()) {
                Some(Ok(filter)) => query.filters.push(filter),
                Some(Err(msg)) => query.error = Some(msg),
                None => match Term::new(raw, opts) {
                    Ok(term) => query.terms.push(term),
                    Err(msg) => query.error = Some(msg),
                },
            }
        }
        query
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    /// A message for the user if part of the query could not be understood
    /// (that part is ignored).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Lowercases text for case-insensitive matching (all of Unicode when the
/// bytes are valid UTF-8, ASCII otherwise).
fn lower(bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_lowercase().into_bytes(),
        Err(_) => bytes.to_ascii_lowercase(),
    }
}

impl Term {
    fn new(raw: &[u8], opts: &QueryOptions) -> Result<Term, String> {
        let mut raw = raw;
        let negate = raw.len() > 1 && raw[0] == b'!';
        if negate {
            raw = &raw[1..];
        }
        let text = String::from_utf8_lossy(raw);
        let ci = match opts.case {
            // In a regex, an escape like \D or \W is not an uppercase letter.
            CaseMode::Smart if opts.regex => {
                let mut escaped = false;
                !text.chars().any(|c| {
                    let was_escaped = escaped;
                    escaped = c == '\\' && !was_escaped;
                    !was_escaped && c.is_uppercase()
                })
            }
            CaseMode::Smart => !text.chars().any(char::is_uppercase),
            CaseMode::Sensitive => false,
            CaseMode::Insensitive => true,
        };
        // On Windows a term may separate folders with `/` or `\`; paths
        // in the index use `\`, so `/` is read as `\`.
        let path = opts.match_path || raw.contains(&b'/') || (cfg!(windows) && !opts.regex && raw.contains(&b'\\'));
        let native: Vec<u8>;
        let (raw, text) = if cfg!(windows) && raw.contains(&b'/') {
            native = raw.iter().map(|&b| if b == b'/' { b'\\' } else { b }).collect();
            // In a regex the backslash must itself be escaped.
            (&native[..], if opts.regex { text.replace('/', r"\\").into() } else { text })
        } else {
            (raw, text)
        };
        let pattern = if opts.regex {
            let regex = regex_lite::RegexBuilder::new(&text).case_insensitive(ci).build();
            Pattern::Regex(regex.map_err(|e| format!("bad regular expression \"{text}\": {e}"))?)
        } else {
            let pat = if ci { lower(raw) } else { raw.to_vec() };
            if raw.iter().any(|&b| b == b'*' || b == b'?') {
                let anywhere = |pat: Vec<u8>| [&b"*"[..], &pat, b"*"].concat();
                Pattern::Glob(if opts.wildcards_anywhere { anywhere(pat) } else { pat })
            } else if opts.whole_word {
                Pattern::Word(pat)
            } else {
                Pattern::Substring(pat)
            }
        };
        Ok(Term { pattern, ci, path, negate })
    }

    /// `scratch` is a reusable buffer for lowercased text.
    fn matches(&self, hay: &[u8], scratch: &mut Vec<u8>) -> bool {
        // ASCII text folds byte by byte as we compare. Anything else is
        // lowercased first (the pattern already is), then compared exactly.
        let (hay, ci) = if self.ci && !hay.is_ascii() && !matches!(self.pattern, Pattern::Regex(_)) {
            scratch.clear();
            scratch.extend(lower(hay));
            (&scratch[..], false)
        } else {
            (hay, self.ci)
        };
        let hit = match &self.pattern {
            Pattern::Substring(pat) => contains(hay, pat, ci),
            Pattern::Word(pat) => contains_word(hay, pat, ci),
            Pattern::Glob(pat) => glob(pat, hay, ci),
            Pattern::Regex(regex) => regex.is_match(&String::from_utf8_lossy(hay)),
        };
        hit != self.negate
    }
}

/// Recognizes `ext:`, `size:` and `modified:` terms. `None` means the term
/// is not a filter; `Some(Err)` that it is one but makes no sense.
fn parse_filter(raw: &[u8], now: i64, zone: &crate::time::TimeZone) -> Option<Result<Filter, String>> {
    let text = std::str::from_utf8(raw).ok()?;
    let (key, value) = text.split_once(':')?;
    let bad = |what: &str| Some(Err(format!("cannot understand {what} filter \"{text}\"")));
    match key.to_ascii_lowercase().as_str() {
        "ext" => {
            let exts: Vec<Vec<u8>> = value
                .split([';', ','])
                .map(|e| e.trim_start_matches('.').to_lowercase().into_bytes())
                .filter(|e| !e.is_empty())
                .collect();
            if exts.is_empty() { bad("the extension") } else { Some(Ok(Filter::Ext(exts))) }
        }
        "size" => match parse_size_range(value) {
            Some((min, max)) => Some(Ok(Filter::Size(min, max))),
            None => bad("the size"),
        },
        "modified" | "datemodified" | "dm" => match parse_date_range(value, now, zone) {
            Some((start, end)) => Some(Ok(Filter::Modified(start, end))),
            None => bad("the date"),
        },
        _ => None,
    }
}

/// `2048`, `10K`, `1.5G`... in bytes; units are powers of 1024.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let digits = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let number: f64 = s[..digits].parse().ok()?;
    let power = match s[digits..].to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 1,
        "m" | "mb" | "mib" => 2,
        "g" | "gb" | "gib" => 3,
        "t" | "tb" | "tib" => 4,
        "p" | "pb" | "pib" => 5,
        _ => return None,
    };
    Some((number * 1024f64.powi(power)) as u64)
}

/// A size condition as an inclusive (min, max) range of bytes.
fn parse_size_range(v: &str) -> Option<(u64, u64)> {
    if let Some(r) = v.strip_prefix(">=") {
        return Some((parse_size(r)?, u64::MAX));
    }
    if let Some(r) = v.strip_prefix("<=") {
        return Some((0, parse_size(r)?));
    }
    if let Some(r) = v.strip_prefix('>') {
        return Some((parse_size(r)?.saturating_add(1), u64::MAX));
    }
    if let Some(r) = v.strip_prefix('<') {
        return Some((0, parse_size(r)?.checked_sub(1)?));
    }
    // A range: `a..b` or `a-b`, either end optional.
    if let Some((a, b)) = v.split_once("..").or_else(|| v.split_once('-')) {
        let min = if a.is_empty() { 0 } else { parse_size(a)? };
        let max = if b.is_empty() { u64::MAX } else { parse_size(b)? };
        return (min <= max && !(a.is_empty() && b.is_empty())).then_some((min, max));
    }
    let exact = parse_size(v.strip_prefix('=').unwrap_or(v))?;
    Some((exact, exact))
}

/// A date condition as [start, end) in Unix seconds, in local time.
fn parse_date_range(v: &str, now: i64, zone: &crate::time::TimeZone) -> Option<(i64, i64)> {
    let (y, m, d, ..) = zone.civil(now);
    let today = zone.midnight(y, m, d);
    // The local midnight `n` calendar days before or after today's.
    let midnight_offset = |days: i64| {
        let (y, m, d, ..) = crate::time::utc_civil(crate::time::utc_civil_days(y, m, d) * 86400 + days * 86400);
        zone.midnight(y, m, d)
    };
    // `YYYY-MM-DD` -> the local midnights that start and end that day.
    let day = |s: &str| -> Option<(i64, i64)> {
        let mut p = s.split('-').map(|n| n.parse::<i64>().ok());
        let (y, m, d) = (p.next()??, p.next()??, p.next()??);
        if p.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let next = crate::time::utc_civil((crate::time::utc_civil_days(y, m, d) + 1) * 86400);
        Some((zone.midnight(y, m, d), zone.midnight(next.0, next.1, next.2)))
    };
    match v.to_ascii_lowercase().as_str() {
        "today" => return Some((today, i64::MAX)),
        "yesterday" => return Some((midnight_offset(-1), today)),
        _ => {}
    }
    if let Some(hours) = v.strip_suffix(['h', 'H']).and_then(|n| n.parse::<i64>().ok()) {
        return Some((now - hours * 3600, i64::MAX));
    }
    if let Some(days) = v.strip_suffix(['d', 'D']).and_then(|n| n.parse::<i64>().ok()) {
        return Some((midnight_offset(-days), i64::MAX));
    }
    if let Some((a, b)) = v.split_once("..") {
        let start = if a.is_empty() { i64::MIN } else { day(a)?.0 };
        let end = if b.is_empty() { i64::MAX } else { day(b)?.1 };
        return (start < end && !(a.is_empty() && b.is_empty())).then_some((start, end));
    }
    // Two full dates joined by a hyphen: 2026-01-01-2026-10-24.
    if v.len() == 21 && v.as_bytes()[10] == b'-' {
        return Some((day(&v[..10])?.0, day(&v[11..])?.1));
    }
    day(v)
}

#[inline]
fn fold(b: u8, ci: bool) -> u8 {
    if ci { b.to_ascii_lowercase() } else { b }
}

/// Offset of the first occurrence of `needle` at or after `from`; `needle`
/// is already lowercased when `ci`.
fn find(hay: &[u8], needle: &[u8], ci: bool, from: usize) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return (from <= hay.len()).then_some(from);
    };
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| {
        fold(hay[i], ci) == first && hay[i + 1..].iter().zip(rest).all(|(&h, &n)| fold(h, ci) == n)
    })
}

/// Substring test; `needle` is already lowercased when `ci`.
fn contains(hay: &[u8], needle: &[u8], ci: bool) -> bool {
    find(hay, needle, ci, 0).is_some()
}

/// Like `contains`, but the match must not touch a letter or digit on
/// either side (any non-ASCII byte counts as part of a word).
fn contains_word(hay: &[u8], needle: &[u8], ci: bool) -> bool {
    let wordy = |b: u8| b.is_ascii_alphanumeric() || b >= 0x80;
    let mut from = 0;
    while let Some(at) = find(hay, needle, ci, from) {
        let end = at + needle.len();
        if (at == 0 || !wordy(hay[at - 1])) && (end == hay.len() || !wordy(hay[end])) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Whole-string wildcard match with single-star backtracking (linear memory,
/// no recursion). `pat` is already lowercased when `ci`.
fn glob(pat: &[u8], text: &[u8], ci: bool) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && pat[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pat.len() && (pat[p] == b'?' || pat[p] == fold(text[t], ci)) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            // Let the last star swallow one more byte and retry.
            star = Some((sp, st + 1));
            p = sp + 1;
            t = st + 1;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&b| b == b'*')
}

impl Filter {
    fn matches(&self, index: &Index, id: u32, scratch: &mut Vec<u8>) -> bool {
        match self {
            Filter::Ext(exts) => {
                if index.is_dir(id) {
                    return false;
                }
                scratch.clear();
                scratch.extend(lower(index.name(id)));
                // "tar.gz" matches "x.tar.gz"; the dot before it is required.
                exts.iter().any(|e| {
                    scratch.len() > e.len() && scratch.ends_with(e) && scratch[scratch.len() - e.len() - 1] == b'.'
                })
            }
            Filter::Size(min, max) => !index.is_dir(id) && (*min..=*max).contains(&index.size(id)),
            Filter::Modified(start, end) => (*start..*end).contains(&index.mtime(id)),
        }
    }
}

impl Index {
    /// Returns the ids of all entries matching `query`.
    pub fn search(&self, query: &Query, opts: &SearchOptions) -> Vec<u32> {
        self.search_until(query, opts, &AtomicBool::new(false)).unwrap_or_default()
    }

    /// As `search`, but gives up (None) soon after `stop` is set: a window
    /// searching as the user types drops a search the next key made
    /// useless.
    pub fn search_until(&self, query: &Query, opts: &SearchOptions, stop: &AtomicBool) -> Option<Vec<u32>> {
        let n = self.len();
        let cores = match opts.threads {
            0 => thread::available_parallelism().map_or(4, |n| n.get()),
            t => t,
        };
        let threads = cores.min(n / 32_768 + 1);

        let mut hits: Vec<u32> = if threads <= 1 {
            self.scan(query, opts.kind, 0..n, stop)
        } else {
            thread::scope(|s| {
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let range = n * t / threads..n * (t + 1) / threads;
                        s.spawn(move || self.scan(query, opts.kind, range, stop))
                    })
                    .collect();
                // Joining in spawn order keeps results in index order.
                handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
            })
        };
        if stop.load(Ordering::Relaxed) {
            return None;
        }

        // Sorting is by (key, id): ties keep index order, as a stable
        // sort would, and every core takes a part.
        let done = match opts.sort {
            Sort::None => true,
            Sort::Name => par_sort(&mut hits, cores, stop, |i| lower(self.name(i))),
            Sort::Path => par_sort(&mut hits, cores, stop, |i| {
                let mut buf = Vec::new();
                self.write_path(i, &mut buf);
                buf
            }),
            Sort::Size => par_sort(&mut hits, cores, stop, |i| self.size(i)),
            Sort::Mtime => par_sort(&mut hits, cores, stop, |i| self.mtime(i)),
        };
        if !done {
            return None;
        }
        if opts.reverse {
            hits.reverse();
        }
        if opts.limit != 0 {
            hits.truncate(opts.limit);
        }
        Some(hits)
    }

    /// True if entry `id` is one `search` would return for `query`.
    pub fn matches(&self, query: &Query, opts: &SearchOptions, id: u32) -> bool {
        !self.scan(query, opts.kind, id as usize..id as usize + 1, &AtomicBool::new(false)).is_empty()
    }

    /// Puts `id` into `hits`, sorted as `search` sorts them, at its place:
    /// for keeping a result list current without searching again.
    pub fn insert_sorted(&self, hits: &mut Vec<u32>, id: u32, opts: &SearchOptions) {
        let mut a_path = Vec::new();
        let mut b_path = Vec::new();
        let mut cmp = |a: u32, b: u32| -> std::cmp::Ordering {
            let by = match opts.sort {
                Sort::None => std::cmp::Ordering::Equal,
                Sort::Name => lower(self.name(a)).cmp(&lower(self.name(b))),
                Sort::Path => {
                    self.write_path(a, &mut a_path);
                    self.write_path(b, &mut b_path);
                    a_path.cmp(&b_path)
                }
                Sort::Size => self.size(a).cmp(&self.size(b)),
                Sort::Mtime => self.mtime(a).cmp(&self.mtime(b)),
            };
            // Ties in index order, as the stable sort leaves them.
            let ord = by.then(a.cmp(&b));
            if opts.reverse { ord.reverse() } else { ord }
        };
        let at = hits.partition_point(|&h| cmp(h, id) == std::cmp::Ordering::Less);
        hits.insert(at, id);
    }

    fn scan(&self, query: &Query, kind: Kind, range: Range<usize>, stop: &AtomicBool) -> Vec<u32> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        let mut scratch = Vec::new();
        let has_path_terms = query.terms.iter().any(|t| t.path);
        for i in range {
            if i % 65_536 == 0 && stop.load(Ordering::Relaxed) {
                return out;
            }
            let id = i as u32;
            if self.flags[i] & crate::index::FLAG_GONE != 0 {
                continue;
            }
            match kind {
                Kind::Any => {}
                Kind::File if self.is_dir(id) => continue,
                Kind::Dir if !self.is_dir(id) => continue,
                _ => {}
            }
            // Cheapest checks first: filters, then name terms; the path
            // is only built for entries that pass both.
            if !query.filters.iter().all(|f| f.matches(self, id, &mut scratch)) {
                continue;
            }
            let name = self.name(id);
            if !query.terms.iter().filter(|t| !t.path).all(|t| t.matches(name, &mut scratch)) {
                continue;
            }
            if has_path_terms {
                self.write_path(id, &mut path);
                if !query.terms.iter().filter(|t| t.path).all(|t| t.matches(&path, &mut scratch)) {
                    continue;
                }
            }
            out.push(id);
        }
        out
    }
}

/// Sorts ids by (key, id) on up to `threads` threads: parts are keyed and
/// sorted at once, then merged in pairs. False if `stop` was set
/// meanwhile (the order is then unfinished).
fn par_sort<K: Ord + Send>(hits: &mut [u32], threads: usize, stop: &AtomicBool, key: impl Fn(u32) -> K + Sync) -> bool {
    let n = hits.len();
    let parts = if n < 65_536 { 1 } else { threads.clamp(1, 64) };
    let size = n.div_ceil(parts).max(1);
    let key = &key;
    let mut runs: Vec<Vec<(K, u32)>> = thread::scope(|s| {
        let handles: Vec<_> = hits
            .chunks(size)
            .map(|part| {
                s.spawn(move || {
                    let mut run: Vec<(K, u32)> = part.iter().map(|&i| (key(i), i)).collect();
                    run.sort_unstable();
                    run
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    while runs.len() > 1 {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let mut pairs = Vec::new();
        let mut iter = runs.into_iter();
        while let Some(a) = iter.next() {
            pairs.push((a, iter.next()));
        }
        runs = thread::scope(|s| {
            let handles: Vec<_> = pairs.into_iter().map(|(a, b)| s.spawn(move || merge(a, b))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
    }
    if let Some(run) = runs.pop() {
        for (slot, (_, id)) in hits.iter_mut().zip(run) {
            *slot = id;
        }
    }
    !stop.load(Ordering::Relaxed)
}

/// Merges two sorted runs into one.
fn merge<K: Ord>(a: Vec<(K, u32)>, b: Option<Vec<(K, u32)>>) -> Vec<(K, u32)> {
    let Some(b) = b else { return a };
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => x <= y,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return out,
        };
        out.push(if take_a { a.next() } else { b.next() }.unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuildOptions;
    use std::fs;

    #[test]
    fn substring_and_glob() {
        assert!(contains(b"Report.PDF", b"port.p", true));
        assert!(!contains(b"Report.PDF", b"port.p", false));
        assert!(contains(b"abc", b"", true));
        assert!(!contains(b"ab", b"abc", true));
        assert!(glob(b"*.pdf", b"a.PDF", true));
        assert!(!glob(b"*.pdf", b"a.pdfx", true));
        assert!(glob(b"a*b*c", b"aXbbYc", false));
        assert!(glob(b"?.rs", b"a.rs", false));
        assert!(!glob(b"?.rs", b"ab.rs", false));
        assert!(glob(b"***", b"", false));
    }

    #[test]
    fn build_save_load_search() {
        let root = std::env::temp_dir().join(format!("ff_core_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("skipped")).unwrap();
        fs::write(root.join("src/Main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/deep/notes.txt"), "hello").unwrap();
        fs::write(root.join("my file.pdf"), "").unwrap();
        fs::write(root.join("skipped/hidden.rs"), "").unwrap();

        let mut opts = BuildOptions::new([&root]);
        opts.excludes.push(root.join("skipped"));
        let built = Index::build(&opts).unwrap();
        let db = root.join("index.db");
        built.save(&db).unwrap();
        let ix = Index::load(&db).unwrap();
        assert_eq!(ix.len(), built.len());

        let find = |q: &str, kind| {
            let o = SearchOptions { kind, sort: Sort::Path, ..Default::default() };
            let root = crate::os::canonicalize(&root).unwrap();
            ix.search(&Query::parse(q), &o)
                .into_iter()
                // Written with `/` whatever the platform's separator.
                .map(|i| ix.path(i).strip_prefix(&root).unwrap().to_str().unwrap().replace('\\', "/"))
                .collect::<Vec<_>>()
        };
        assert_eq!(find("main", Kind::Any), ["src/Main.rs"]);
        assert_eq!(find("Main", Kind::Any), ["src/Main.rs"]);
        assert!(find("MAIN", Kind::Any).is_empty()); // smart case
        assert_eq!(find("*.rs", Kind::Any), ["src/Main.rs"]); // skipped/ excluded
        assert_eq!(find("\"my file\"", Kind::Any), ["my file.pdf"]);
        assert_eq!(find("src/deep/ !deep", Kind::Any), ["src/deep/notes.txt"]);
        assert_eq!(find("dee", Kind::Dir), ["src/deep"]);
        // An excluded directory is listed itself, but never descended into.
        assert_eq!(find("skipped", Kind::Any), ["skipped"]);
        assert_eq!(find("txt", Kind::File), ["src/deep/notes.txt"]);

        let id = ix.search(&Query::parse("notes.txt"), &Default::default())[0];
        assert_eq!(ix.size(id), 5);
        assert!(!ix.is_dir(id));

        fs::write(&db, b"garbage").unwrap();
        assert!(Index::load(&db).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn whole_words_and_unicode_case() {
        assert!(contains_word(b"my report.pdf", b"report", false));
        assert!(contains_word(b"report", b"report", false));
        assert!(contains_word(b"a-report_2", b"report", false)); // _ and - separate words
        assert!(!contains_word(b"reports.pdf", b"report", false));
        assert!(!contains_word(b"myreport.pdf", b"report", false));
        assert!(contains_word(b"ports report", b"port", false) == false);
        assert!(contains_word(b"xport port", b"port", false)); // a later whole-word hit counts

        let opts = QueryOptions::default();
        let mut scratch = Vec::new();
        let mut hit = |term: &str, name: &str| Term::new(term.as_bytes(), &opts).unwrap().matches(name.as_bytes(), &mut scratch);
        assert!(hit("école", "ÉCOLE.txt"));
        assert!(hit("straße", "STRASSE") == false); // simple lowercasing, not full folding
        assert!(hit("ÉCOLE", "école.txt") == false); // an uppercase letter makes it exact
        assert!(hit("заметки", "Мои ЗАМЕТКИ.txt"));
        assert!(hit("*.jpég", "PHOTO.JPÉG"));
        assert!(hit("report", "Été REPORT.pdf")); // ASCII term in a non-ASCII name
    }

    #[test]
    fn path_matching_and_loose_wildcards() {
        let mut scratch = Vec::new();
        let anywhere = QueryOptions { wildcards_anywhere: true, ..Default::default() };
        let strict = QueryOptions::default();
        let mut hit = |term: &str, opts: &QueryOptions, name: &str| Term::new(term.as_bytes(), opts).unwrap().matches(name.as_bytes(), &mut scratch);
        assert!(!hit("rep*t", &strict, "my report.pdf")); // must cover the whole name
        assert!(hit("rep*t", &anywhere, "my report.pdf"));
        assert!(hit("*.pdf", &anywhere, "my report.pdf"));
        // With match_path every term looks at the full path.
        let by_path = QueryOptions { match_path: true, ..Default::default() };
        assert!(Term::new(b"docs", &by_path).unwrap().path);
        assert!(!Term::new(b"docs", &strict).unwrap().path);
        assert!(Term::new(b"a/b", &strict).unwrap().path);
    }

    #[test]
    fn regular_expressions() {
        let regex = QueryOptions { regex: true, ..Default::default() };
        let mut scratch = Vec::new();
        let mut hit = |term: &str, name: &str| Term::new(term.as_bytes(), &regex).unwrap().matches(name.as_bytes(), &mut scratch);
        assert!(hit(r"^holiday.*\.jpg$", "Holiday 2026.JPG")); // no uppercase letter: ignores case
        assert!(!hit(r"^holiday.*\.jpg$", "my holiday.jpg"));
        assert!(hit(r"^IMG_\d+\.png$", "IMG_0042.png"));
        assert!(!hit(r"^IMG_\d+\.png$", "img_0042.png")); // uppercase letters: exact case
        assert!(hit(r"\d{4}-\d\d", "notes 2026-11.txt")); // \d alone does not force exact case
        assert!(Term::new(b"(unclosed", &regex).is_err());
        let q = Query::parse_opts("(unclosed", &regex);
        assert!(q.error().unwrap().contains("regular expression"));
    }

    #[test]
    fn size_filters() {
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("1M"), Some(1 << 20));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("7X"), None);
        assert_eq!(parse_size_range("2048"), Some((2048, 2048)));
        assert_eq!(parse_size_range("=2048B"), Some((2048, 2048)));
        assert_eq!(parse_size_range(">1K"), Some((1025, u64::MAX)));
        assert_eq!(parse_size_range(">=1K"), Some((1024, u64::MAX)));
        assert_eq!(parse_size_range("<1K"), Some((0, 1023)));
        assert_eq!(parse_size_range("<=1K"), Some((0, 1024)));
        assert_eq!(parse_size_range("10M..100M"), Some((10 << 20, 100 << 20)));
        assert_eq!(parse_size_range("10M-100M"), Some((10 << 20, 100 << 20)));
        assert_eq!(parse_size_range("10M.."), Some((10 << 20, u64::MAX)));
        assert_eq!(parse_size_range("..100M"), Some((0, 100 << 20)));
        assert_eq!(parse_size_range("-100M"), Some((0, 100 << 20)));
        assert_eq!(parse_size_range("100M..10M"), None);
        assert_eq!(parse_size_range("big"), None);
        assert_eq!(parse_size_range(".."), None);
    }

    #[test]
    fn date_filters() {
        use crate::time::TimeZone;
        // A zone file gives real local days; fall back to UTC without one.
        let zone = std::fs::read("/usr/share/zoneinfo/Europe/Paris").ok().and_then(|d| TimeZone::parse(&d)).unwrap_or_default();
        let at = |y, m, d, h: i64| zone.midnight(y, m, d) + h * 3600;
        let now = at(2026, 7, 16, 15); // 15:00 local on 16 July 2026
        let range = |v: &str| parse_date_range(v, now, &zone);
        assert_eq!(range("today"), Some((at(2026, 7, 16, 0), i64::MAX)));
        assert_eq!(range("yesterday"), Some((at(2026, 7, 15, 0), at(2026, 7, 16, 0))));
        assert_eq!(range("3h"), Some((now - 3 * 3600, i64::MAX)));
        assert_eq!(range("7d"), Some((at(2026, 7, 9, 0), i64::MAX)));
        assert_eq!(range("2026-07-01"), Some((at(2026, 7, 1, 0), at(2026, 7, 2, 0))));
        assert_eq!(range("2026-01-01..2026-07-01"), Some((at(2026, 1, 1, 0), at(2026, 7, 2, 0))));
        assert_eq!(range("2026-01-01-2026-07-01"), Some((at(2026, 1, 1, 0), at(2026, 7, 2, 0))));
        assert_eq!(range("2026-01-01.."), Some((at(2026, 1, 1, 0), i64::MAX)));
        assert_eq!(range("..2026-07-01"), Some((i64::MIN, at(2026, 7, 2, 0))));
        assert_eq!(range("2026-12-31"), Some((at(2026, 12, 31, 0), at(2027, 1, 1, 0))));
        assert_eq!(range("2026-13-01"), None);
        assert_eq!(range("soon"), None);
        assert_eq!(range("2026-07-02..2026-07-01"), None);
    }

    #[test]
    fn filters_in_queries() {
        let root = std::env::temp_dir().join(format!("ff_filter_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("movie.mp4")).unwrap(); // a folder named like a file
        fs::write(root.join("clip.MP4"), vec![0u8; 3000]).unwrap();
        fs::write(root.join("song.mp3"), vec![0u8; 500]).unwrap();
        fs::write(root.join("backup.tar.gz"), vec![0u8; 10]).unwrap();
        fs::write(root.join("notes.gz"), vec![0u8; 20]).unwrap();
        fs::write(root.join("targz"), "").unwrap();
        let ix = Index::build(&BuildOptions::new([&root])).unwrap();
        let find = |q: &str| {
            let query = Query::parse(q);
            assert!(query.error().is_none(), "{q}: {:?}", query.error());
            let mut names: Vec<String> = ix
                .search(&query, &SearchOptions::default())
                .into_iter()
                .map(|i| String::from_utf8_lossy(ix.name(i)).into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(find("ext:mp4"), ["clip.MP4"]); // case-insensitive; the folder is not a file
        assert_eq!(find("ext:.mp4;mp3"), ["clip.MP4", "song.mp3"]);
        assert_eq!(find("ext:mp4 ext:mp3"), [] as [&str; 0]); // separate filters must all match
        assert_eq!(find("ext:gz"), ["backup.tar.gz", "notes.gz"]);
        assert_eq!(find("ext:tar.gz"), ["backup.tar.gz"]);
        assert_eq!(find("size:>1K"), ["clip.MP4"]);
        assert_eq!(find("size:10..500"), ["backup.tar.gz", "notes.gz", "song.mp3"]);
        assert_eq!(find("size:<=20 ext:gz"), ["backup.tar.gz", "notes.gz"]);
        assert_eq!(find("clip modified:today"), ["clip.MP4"]);
        assert_eq!(find("clip dm:1h"), ["clip.MP4"]);
        assert_eq!(find("clip modified:..2001-01-01"), [] as [&str; 0]);
        assert!(Query::parse("size:huge").error().is_some());
        assert!(Query::parse("modified:someday").error().is_some());
        assert!(Query::parse("other:thing").error().is_none()); // not a filter: plain text
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hidden_names_can_be_left_out() {
        use crate::live::LiveIndex;
        use std::collections::HashSet;
        let root = std::env::temp_dir().join(format!("ff_hidden_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".config/app")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join(".config/app/settings.ini"), "").unwrap();
        fs::write(root.join(".profile"), "").unwrap();
        fs::write(root.join("docs/visible.txt"), "").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let count = |ix: &Index, q: &str| ix.search(&Query::parse(q), &SearchOptions::default()).len();

        let all = Index::build(&BuildOptions::new([&root])).unwrap();
        assert_eq!((count(&all, "settings.ini"), count(&all, ".profile"), all.skips_hidden()), (1, 1, false));

        let mut opts = BuildOptions::new([&root]);
        opts.skip_hidden = true;
        let lean = Index::build(&opts).unwrap();
        assert_eq!((count(&lean, "settings.ini"), count(&lean, ".profile"), count(&lean, ".config")), (0, 0, 0));
        assert_eq!(count(&lean, "visible.txt"), 1);
        assert!(lean.skips_hidden());
        // The marker is not a folder, and survives saving and refreshing.
        assert!(lean.excludes().all(|x| x.is_absolute()));
        lean.save(root.join("i.db")).unwrap();
        let reloaded = Index::load(root.join("i.db")).unwrap();
        assert!(BuildOptions::refresh(&reloaded).skip_hidden);

        // Live updates respect it too.
        let mut live = LiveIndex::new(reloaded);
        fs::write(root.join(".newhidden"), "").unwrap();
        fs::write(root.join(".config/app/more.ini"), "").unwrap();
        fs::write(root.join("docs/shown.txt"), "").unwrap();
        let change = |dir: &std::path::Path, name: &str| (crate::os::path_bytes(dir).to_vec(), name.as_bytes().to_vec());
        let batch = HashSet::from([
            change(&root, ".newhidden"),
            change(&root.join(".config/app"), "more.ini"),
            change(&root.join("docs"), "shown.txt"),
        ]);
        assert!(live.apply(&batch));
        assert_eq!((count(live.index(), ".newhidden"), count(live.index(), "more.ini")), (0, 0));
        assert_eq!(count(live.index(), "shown.txt"), 1);
        fs::remove_dir_all(&root).unwrap();
    }
}
