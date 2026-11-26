//! Linux and other Unix systems: one running window per user. The first
//! start listens on a socket in the user's runtime folder; a second start
//! (such as the global shortcut) asks it to show its window, which has
//! the index loaded, and exits.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("ff-gui.sock"),
        // No runtime folder: a per-user name in /tmp.
        None => std::env::temp_dir().join(format!("ff-gui-{}.sock", std::env::var("USER").unwrap_or_default())),
    }
}

/// True if a running copy took the request to show its window.
pub fn wake_running() -> bool {
    UnixStream::connect(socket_path()).and_then(|mut s| s.write_all(b"show\n")).is_ok()
}

/// True if a copy already runs.
pub fn running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

/// Starts listening for later starts; each one sends `()` on the
/// returned channel. None if the socket cannot be made.
pub fn listen() -> Option<mpsc::Receiver<()>> {
    let path = socket_path();
    // Left over from a copy that did not exit cleanly: nobody answers it.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() && tx.send(()).is_err() {
                break;
            }
        }
    });
    Some(rx)
}
