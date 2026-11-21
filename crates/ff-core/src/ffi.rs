//! C API. See `include/ff.h` for the documented declarations.
//!
//! All functions tolerate NULL handles (returning NULL / 0 / -1). Strings are
//! NUL-terminated byte strings; paths need not be valid UTF-8.

use std::ffi::{CStr, c_char, c_int};
use std::path::PathBuf;

use crate::{BuildOptions, CaseMode, Index, Kind, Query, SearchOptions, Sort};

/// Search results; borrows the index it came from.
pub struct FfResults {
    index: *const Index,
    ids: Vec<u32>,
}

unsafe fn path_arg(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    Some(crate::os::path_from_bytes(unsafe { CStr::from_ptr(p) }.to_bytes()))
}

unsafe fn path_list(list: *const *const c_char, n: usize) -> Vec<PathBuf> {
    if list.is_null() {
        return Vec::new();
    }
    (0..n).filter_map(|i| unsafe { path_arg(*list.add(i)) }).collect()
}

fn boxed(index: std::io::Result<Index>) -> *mut Index {
    index.map_or(std::ptr::null_mut(), |ix| Box::into_raw(Box::new(ix)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_build(
    roots: *const *const c_char,
    n_roots: usize,
    excludes: *const *const c_char,
    n_excludes: usize,
) -> *mut Index {
    let mut opts = BuildOptions::new(unsafe { path_list(roots, n_roots) });
    opts.excludes.extend(unsafe { path_list(excludes, n_excludes) });
    boxed(Index::build(&opts))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_load(path: *const c_char) -> *mut Index {
    match unsafe { path_arg(path) } {
        Some(p) => boxed(Index::load(p)),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_save(index: *const Index, path: *const c_char) -> c_int {
    match unsafe { (index.as_ref(), path_arg(path)) } {
        (Some(ix), Some(p)) if ix.save(&p).is_ok() => 0,
        _ => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_len(index: *const Index) -> usize {
    unsafe { index.as_ref() }.map_or(0, Index::len)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_free(index: *mut Index) {
    if !index.is_null() {
        drop(unsafe { Box::from_raw(index) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_search(
    index: *const Index,
    query: *const c_char,
    kind: c_int,
    sort: c_int,
    reverse: c_int,
    limit: usize,
) -> *mut FfResults {
    let Some(ix) = (unsafe { index.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let text = if query.is_null() {
        String::new()
    } else {
        // Lossy only affects the term splitter's view of invalid UTF-8.
        unsafe { CStr::from_ptr(query) }.to_string_lossy().into_owned()
    };
    let opts = SearchOptions {
        kind: match kind {
            1 => Kind::File,
            2 => Kind::Dir,
            _ => Kind::Any,
        },
        sort: match sort {
            1 => Sort::Name,
            2 => Sort::Path,
            3 => Sort::Size,
            4 => Sort::Mtime,
            _ => Sort::None,
        },
        reverse: reverse != 0,
        limit,
        threads: 0,
    };
    let ids = ix.search(&Query::parse_with(&text, CaseMode::Smart), &opts);
    Box::into_raw(Box::new(FfResults { index, ids }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_len(results: *const FfResults) -> usize {
    unsafe { results.as_ref() }.map_or(0, |r| r.ids.len())
}

unsafe fn entry<'a>(results: *const FfResults, i: usize) -> Option<(&'a Index, u32)> {
    let r = unsafe { results.as_ref() }?;
    Some((unsafe { &*r.index }, *r.ids.get(i)?))
}

/// Copies the path of result `i` into `buf` (always NUL-terminated when
/// `cap > 0`, truncated if needed) and returns the full path length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_path(
    results: *const FfResults,
    i: usize,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    let Some((ix, id)) = (unsafe { entry(results, i) }) else {
        return 0;
    };
    let mut path = Vec::new();
    ix.write_path(id, &mut path);
    if !buf.is_null() && cap > 0 {
        let n = path.len().min(cap - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(path.as_ptr(), buf.cast::<u8>(), n);
            *buf.add(n) = 0;
        }
    }
    path.len()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_size(results: *const FfResults, i: usize) -> u64 {
    unsafe { entry(results, i) }.map_or(0, |(ix, id)| ix.size(id))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_mtime(results: *const FfResults, i: usize) -> i64 {
    unsafe { entry(results, i) }.map_or(0, |(ix, id)| ix.mtime(id))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_is_dir(results: *const FfResults, i: usize) -> c_int {
    unsafe { entry(results, i) }.map_or(0, |(ix, id)| ix.is_dir(id) as c_int)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_results_free(results: *mut FfResults) {
    if !results.is_null() {
        drop(unsafe { Box::from_raw(results) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn roundtrip_through_c_api() {
        let dir = std::env::temp_dir().join(format!("ff_ffi_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("needle.txt"), "abc").unwrap();
        let root = CString::new(crate::os::path_bytes(&dir)).unwrap();
        let roots = [root.as_ptr()];
        unsafe {
            let ix = ff_index_build(roots.as_ptr(), 1, std::ptr::null(), 0);
            assert_eq!(ff_index_len(ix), 2);
            let res = ff_search(ix, c"NEEDLE*".as_ptr(), 1, 0, 0, 0);
            assert_eq!(ff_results_len(res), 0); // smart case: uppercase is exact
            ff_results_free(res);
            let res = ff_search(ix, c"needle*".as_ptr(), 1, 0, 0, 0);
            assert_eq!(ff_results_len(res), 1);
            assert_eq!(ff_results_size(res, 0), 3);
            assert_eq!(ff_results_is_dir(res, 0), 0);
            let mut buf = [0 as c_char; 4096];
            let len = ff_results_path(res, 0, buf.as_mut_ptr(), buf.len());
            let path = CStr::from_ptr(buf.as_ptr()).to_bytes();
            assert_eq!(path.len(), len);
            assert!(path.ends_with(&[&[crate::os::SEP][..], b"needle.txt"].concat()));
            assert_eq!(ff_results_path(res, 0, buf.as_mut_ptr(), 4), len);
            assert_eq!(CStr::from_ptr(buf.as_ptr()).to_bytes().len(), 3);
            ff_results_free(res);
            ff_index_free(ix);
            assert_eq!(ff_index_len(std::ptr::null()), 0);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
