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
                prompt: Some(crate::i18n::t!("pickers_dialogs.file_dialog.choose_file").into()),
            })
        });
        let mut paths = match receiver.await {
            Ok(Ok(paths)) => paths?,
            _ => return None,
        };
        paths.pop()
    }
}

/// What follows the file pass of a two-pass picker.
enum AfterFilePass {
    /// The user picked files: that set is the answer.
    Done(Vec<PathBuf>),
    /// Cancelled or empty — offer the folder pass. Declining the file dialog
    /// is how a folder gets picked, so this pass is offered, not skipped.
    Folders,
    /// The dialog itself failed (no portal, no OS dialog). Reported as-is:
    /// retrying the same failure as a folder picker only doubles it.
    Failed(anyhow::Error),
}

/// Pick one file AND/OR folder set. Windows and Linux accept one mode per
/// dialog (Windows's `FOS_PICKFOLDERS` is exclusive; the XDG portal picks
/// files *or* directories), so those platforms get two sequential passes:
/// files first, then folders once the file pass is cancelled or comes back
/// empty — two cancels to reach “never mind”. macOS mixes both in one
/// dialog and keeps that single pass.
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
        let next = match files.await {
            Ok(outcome) => after_file_pass(outcome),
            // The dialog's sender went away (the window closed mid-pick):
            // answer by dropping ours, i.e. "cancelled", rather than opening
            // a second dialog nobody asked for.
            Err(_) => return,
        };
        let reply = match next {
            AfterFilePass::Done(paths) => Ok(Some(paths)),
            AfterFilePass::Failed(error) => Err(error),
            AfterFilePass::Folders => {
                let folders = cx.update(|cx| {
                    cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple,
                        prompt: Some(prompt.into()),
                    })
                });
                match folders.await {
                    Ok(Ok(paths)) => Ok(paths),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Ok(None),
                }
            }
        };
        let _ = done_tx.send(reply);
    })
    .detach();
    done_rx
}

/// See [`AfterFilePass`].
fn after_file_pass(outcome: anyhow::Result<Option<Vec<PathBuf>>>) -> AfterFilePass {
    match outcome {
        Ok(Some(paths)) if !paths.is_empty() => AfterFilePass::Done(paths),
        Ok(_) => AfterFilePass::Folders,
        Err(error) => AfterFilePass::Failed(error),
    }
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
            // SAFETY: `c` is NUL-terminated and outlives the call;
            // `stringWithUTF8String:` copies the bytes into an autoreleased
            // NSString that the caller immediately hands to `addObject:`.
            Ok(c) => unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] },
            Err(_) => std::ptr::null_mut(),
        }
    }

    // SAFETY: all selectors are documented NSOpenPanel/NSString methods,
    // arguments are bools or objects created above, and every pointer the
    // panel returns is null-checked before use; `UTF8String`'s buffer is
    // copied out via `CStr::to_string_lossy` before the next release-pool
    // drain.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_pass_pick_only_ends_on_files_that_came_back() {
        let picked = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/dir")];
        assert!(matches!(
            after_file_pass(Ok(Some(picked.clone()))),
            AfterFilePass::Done(paths) if paths == picked
        ));
        // Cancelled and empty both mean "the file pass declined" — the
        // folder pass is how a folder is chosen at all.
        assert!(matches!(after_file_pass(Ok(None)), AfterFilePass::Folders));
        assert!(matches!(
            after_file_pass(Ok(Some(Vec::new()))),
            AfterFilePass::Folders
        ));
        // A failing dialog is reported, never retried as a folder picker.
        let failed = after_file_pass(Err(anyhow::anyhow!("no portal")));
        assert!(matches!(
            failed,
            AfterFilePass::Failed(error) if error.to_string() == "no portal"
        ));
    }
}
