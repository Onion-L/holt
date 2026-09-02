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
        // stay in the binary's static data. NSImage has no class factory for
        // data (that's UIKit's `UIImage imageWithData:`) — AppKit wants the
        // `alloc` + `initWithData:` instance-initializer pair; a wrong
        // selector here raises an NSException that unwinds through the
        // extern-C frame and aborts the whole app.
        let bytes = APP_ICON_PNG.as_ptr() as *const std::ffi::c_void;
        let length = APP_ICON_PNG.len();
        let data: *mut Object = msg_send![class!(NSData), dataWithBytes: bytes length: length];
        if data.is_null() {
            return;
        }
        let alloc: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![alloc, initWithData: data];
        if image.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install() {}
