//! Native file picker for settings fields. macOS runs a real `NSOpenPanel`
//! (blocking, from the UI thread — AppKit's `runModal` nested event loop is
//! how a modal panel works in any AppKit app, so the dialog stays interactive
//! and the app keeps compositing behind it). Other platforms return `None`
//! and callers keep their manual-entry affordance.

use std::path::PathBuf;

/// Pick one image file. `None` on cancel, or on platforms without a native
/// dialog.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc 0.2's msg_send carries a cfg(cargo-clippy) branch
pub fn pick_image() -> Option<PathBuf> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;

    fn ns_string(value: &str) -> *mut Object {
        match CString::new(value) {
            Ok(c) => unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] },
            Err(_) => std::ptr::null_mut(),
        }
    }

    unsafe {
        let panel: *mut Object = msg_send![class!(NSOpenPanel), openPanel];
        if panel.is_null() {
            return None;
        }
        let _: () = msg_send![panel, setCanChooseFiles: true];
        let _: () = msg_send![panel, setCanChooseDirectories: false];
        let _: () = msg_send![panel, setAllowsMultipleSelection: false];
        // Extension filter rather than the UTType `setAllowedContentTypes`
        // API: dynamic messaging needs no extra framework link. The list is
        // the backdrop decoder's contract, shared with the drop targets.
        let types: *mut Object = msg_send![class!(NSMutableArray), array];
        for &extension in crate::chat_backdrop::SUPPORTED_EXTENSIONS {
            let s = ns_string(extension);
            let _: () = msg_send![types, addObject: s];
        }
        let _: () = msg_send![panel, setAllowedFileTypes: types];

        // NSModalResponseOK
        let response: isize = msg_send![panel, runModal];
        if response != 1 {
            return None;
        }
        let urls: *mut Object = msg_send![panel, URLs];
        let url: *mut Object = msg_send![urls, firstObject];
        if url.is_null() {
            return None;
        }
        let path: *mut Object = msg_send![url, path];
        if path.is_null() {
            return None;
        }
        let utf8: *const c_char = msg_send![path, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(PathBuf::from(
            CStr::from_ptr(utf8).to_string_lossy().into_owned(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pick_image() -> Option<PathBuf> {
    None
}
