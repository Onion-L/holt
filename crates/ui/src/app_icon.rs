//! The desktop app icon: the bundled mark, pushed onto NSApplication at
//! boot so the Dock, ⌘-Tab, and About surfaces carry it. A raw cargo-run
//! binary has no `.app` bundle to hold an `.icns`, so the icon is set at
//! runtime instead — the same one-object-few-messages objc shape
//! `appearance` uses for NSAppearance.

const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");

#[cfg(target_os = "macos")]
pub(crate) fn install() {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        // `+[NSData dataWithBytes:length:]` copies the bytes, so the PNG can
        // stay in the binary's static data; `NSImage initWithData:` parses
        // the PNG without us owning a decoder.
        let bytes = APP_ICON_PNG.as_ptr() as *const std::ffi::c_void;
        let length = APP_ICON_PNG.len();
        let data: *mut Object = msg_send![class!(NSData), dataWithBytes: bytes length: length];
        if data.is_null() {
            return;
        }
        let image: *mut Object = msg_send![class!(NSImage), initWithData: data];
        if image.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install() {}
