//! Optional diagnostics: lines appended to the file named by FF_GUI_DEBUG.

#[cfg_attr(windows, allow(dead_code))] // only the X11 backend asks
pub fn enabled() -> bool {
    std::env::var_os("FF_GUI_DEBUG").is_some()
}

pub fn log(msg: impl AsRef<str>) {
    let Some(path) = std::env::var_os("FF_GUI_DEBUG") else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = std::io::Write::write_all(&mut file, format!("{}\n", msg.as_ref()).as_bytes());
    }
}
