//! System-trash deletion for the File sidebar (ticket 07). Deletion goes
//! through the operating system's trash and NOTHING else: a platform without
//! a served trash, or a trash operation that fails, reports the failure —
//! there is no permanent-delete fallback anywhere in this module.

use std::path::Path;

/// Move one entry (file, directory, or symlink — the link itself) to the
/// system trash. Returns `Err` with a user-presentable message when the
/// trash is unavailable or refuses; the entry is never deleted otherwise.
pub(crate) fn move_to_trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos_move_to_trash(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err("The system trash is not available on this platform.".into())
    }
}

/// macOS: `NSFileManager trashItemAtURL:resultingItemURL:error:` — the
/// synchronous Finder-equivalent move to `~/.Trash` (or the volume's trash),
/// with the error reported straight back. Unlike `NSWorkspace
/// recycleURLs:` it needs no completion block and no main-thread hop, which
/// is why it is the `trash` crate's non-Finder default too. Its one known
/// quirk — "Put Back" sometimes missing in Finder's context menu — does not
/// affect recoverability: the entry is in the Trash and can be dragged out.
#[cfg(target_os = "macos")]
fn macos_move_to_trash(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    // NSURL is built from an NSString, so a non-UTF-8 spelling cannot be
    // addressed safely — refuse rather than trash the wrong entry.
    let Some(spelling) = path.to_str() else {
        return Err("The entry's name is not valid UTF-8.".into());
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(spelling));
    let manager = NSFileManager::defaultManager();
    manager
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|error| {
            let reason = error.localizedDescription().to_string();
            format!("Could not move to the Trash: {reason}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn trashing_moves_the_entry_to_the_system_trash() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trash-me.txt");
        std::fs::write(&file, b"contents").unwrap();
        move_to_trash(&file).expect("the local volume serves a trash");
        assert!(
            !file.exists(),
            "the entry left its source location after a successful trash"
        );
    }

    #[test]
    fn trash_failures_report_and_leave_the_entry_alone() {
        let dir = tempfile::tempdir().unwrap();
        // A missing entry fails — and nothing else is touched, deleted, or
        // created by the attempt.
        let missing = dir.path().join("not-here.txt");
        assert!(move_to_trash(&missing).is_err());
        assert!(!missing.exists());
    }
}
