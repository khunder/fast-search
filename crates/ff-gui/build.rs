//! Embeds the application icon in ff-gui.exe, so Explorer, the taskbar
//! and shortcuts show it. The icon comes from the same code the window
//! draws it with.

#[path = "src/app_icon.rs"]
mod app_icon;

fn main() {
    println!("cargo:rerun-if-changed=src/app_icon.rs");
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    // MSVC's linker and lld take a compiled resource file directly; the
    // MinGW linker would need it converted first, so it goes without.
    if target("CARGO_CFG_TARGET_OS") != "windows" || target("CARGO_CFG_TARGET_ENV") != "msvc" {
        return;
    }
    let res = std::path::Path::new(&target("OUT_DIR")).join("ff-gui.res");
    std::fs::write(&res, app_icon::windows_resource()).expect("writing the icon resource");
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
