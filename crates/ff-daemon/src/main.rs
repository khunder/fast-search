//! ffd: keeps the ff index current.
//!
//! On Linux it runs as root and follows whole filesystems with fanotify
//! (`linux.rs`). On Windows it runs as a service (or in a console) and
//! follows each indexed drive with `ReadDirectoryChangesW` (`windows.rs`).
//! Either way the index is published to a file that the unprivileged `ff`
//! and `ff-gui` read; they never talk to this process.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("ffd supports Linux and Windows only");

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    return linux::main();
    #[cfg(windows)]
    return windows::main();
}
