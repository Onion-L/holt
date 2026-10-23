//! Native file pickers for settings fields, plus one mixed files-or-dirs
//! picker shared by the composer and theme import. macOS runs a real
//! `NSOpenPanel` inside the caller's spawned task (blocking, from the UI
//! thread — AppKit's `runModal` nested event loop is how a modal panel
//! works in any AppKit app, and running it inside a gpui dispatch would
//! collide with held borrows; see the callers). Other platforms request
//! gpui's own dialog, which carries no extension filter — a wrong choice
//! surfaces where the bytes are read.

use std::path::PathBuf;

/// Extensions a custom provider logo may be picked from — the engine's
/// normalizer accepts these rasters and SVG.
pub const LOGO_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "svg"];

/// Pick one image file (the chat backdrop). `None` on cancel.
pub async fn pick_image(cx: &mut gpui::AsyncApp) -> Option<PathBuf> {
    pick_file(crate::chat_backdrop::SUPPORTED_EXTENSIONS, cx).await
}

/// Pick one custom provider logo. `None` on cancel.
pub async fn pick_logo(cx: &mut gpui::AsyncApp) -> Option<PathBuf> {
    pick_file(LOGO_EXTENSIONS, cx).await
}

/// Pick one file with one of `extensions`.
async fn pick_file(extensions: &[&str], cx: &mut gpui::AsyncApp) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let _ = cx;
        pick_file_with_panel(extensions)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = extensions;
        let receiver = cx.update(|cx| {
            cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Choose File".into()),
            })
        });
        let mut paths = match receiver.await {
            Ok(Ok(paths)) => paths?,
            _ => return None,
        };
        paths.pop()
    }
}

/// Pick one file AND/OR folder set. Platforms that cannot mix files and
/// folders in one dialog (Windows — its folder mode is exclusive) get two
/// sequential passes: files first, then folders once the file pass is
/// cancelled or comes back empty. macOS and Linux keep the single mixed
/// dialog they always had, byte for byte.
pub fn prompt_for_files_and_dirs(
    prompt: &str,
    multiple: bool,
    cx: &mut gpui::App,
) -> futures::channel::oneshot::Receiver<anyhow::Result<Option<Vec<PathBuf>>>> {
    if cx.can_select_mixed_files_and_dirs() {
        return cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple,
            prompt: Some(prompt.into()),
        });
    }
    let files = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple,
        prompt: Some(prompt.into()),
    });
    let prompt = prompt.to_string();
    let (done_tx, done_rx) = futures::channel::oneshot::channel();
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = files.await
            && !paths.is_empty()
        {
            let _ = done_tx.send(Ok(Some(paths)));
            return;
        }
        // Cancelled or empty — offer the folder pass.
        let folders = cx.update(|cx| {
            cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple,
                prompt: Some(prompt.into()),
            })
        });
        let reply = match folders.await {
            Ok(Ok(paths)) => Ok(paths),
            Ok(Err(error)) => Err(error),
            Err(_) => Ok(None),
        };
        let _ = done_tx.send(reply);
    })
    .detach();
    done_rx
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc 0.2's msg_send carries a cfg(cargo-clippy) branch
fn pick_file_with_panel(extensions: &[&str]) -> Option<PathBuf> {
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
        // API: dynamic messaging needs no extra framework link.
        let types: *mut Object = msg_send![class!(NSMutableArray), array];
        for &extension in extensions {
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
