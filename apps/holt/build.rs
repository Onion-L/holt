//! The binary's Windows resource stamp: the app icon embedded as the exe's
//! first RT_GROUP_ICON — the taskbar, Alt-Tab, and Explorer all read it from
//! there (macOS gets its icon from the .app bundle instead, so this is
//! windows-only).

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    #[cfg(target_os = "windows")]
    {
        if let Err(error) = WindowsResource::new().set_icon("assets/app.ico").compile() {
            panic!("embedding the app icon failed: {error:?}");
        }
    }
}

#[cfg(target_os = "windows")]
use winresource::WindowsResource;
