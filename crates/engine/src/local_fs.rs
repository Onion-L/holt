//! Local-machine surfaces for the add-space palette: folder browsing,
//! mounted volumes, hostname, and the local device row.

use std::path::Path;

use holt_proto::{Device, DriveEntry, DriveListing, FolderEntry, FolderListing};

/// The local machine as a device row — enough for the add-space palette to
/// browse this computer (presence for the local device is always "online").
pub(crate) fn local_device(device_id: &str) -> Device {
    Device {
        id: device_id.to_string(),
        name: hostname(),
        platform: std::env::consts::OS.to_string(),
        last_seen_at: None,
        created_at: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
}

/// Bare hostname without any domain suffix ("macbook.local" → "macbook").
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]);
        let name = name.split('.').next().unwrap_or("").trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "This device".to_string()
}

pub(crate) fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .map(|home| home.to_string_lossy().to_string())
        .filter(|home| !home.is_empty())
}

/// The UI expands `~` itself; tolerate it here anyway so the method is
/// callable without the shell's helpers.
pub(crate) fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Browse cap — the palette scrolls, but a runaway directory (think `/`) is
/// still bounded; `truncated` tells the UI some entries were dropped.
const FOLDER_ENTRY_CAP: usize = 500;

pub(crate) fn list_folders(params: &serde_json::Value) -> Result<FolderListing, String> {
    let requested = params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let path = match requested {
        Some(path) => expand_tilde(path),
        None => home_dir().ok_or_else(|| "could not resolve your home folder".to_string())?,
    };
    let read =
        std::fs::read_dir(&path).map_err(|error| format!("could not read that folder: {error}"))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().to_string();
        // Dotfiles stay hidden — the browser is for project folders.
        if name.starts_with('.') {
            continue;
        }
        // `std::fs::metadata` (not `item.metadata`) so a symlinked folder
        // still counts as a folder.
        let Ok(meta) = std::fs::metadata(item.path()) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if entries.len() >= FOLDER_ENTRY_CAP {
            truncated = true;
            break;
        }
        entries.push(FolderEntry {
            is_repo: item.path().join(".git").exists(),
            name,
            is_dir: true,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(FolderListing {
        path,
        entries,
        truncated,
    })
}

/// Browse roots beyond home: the system root plus mounted volumes. macOS
/// keeps mounts under `/Volumes`; the boot volume's alias there resolves to
/// `/`, so it is deduped against the System row.
pub(crate) fn list_drives() -> DriveListing {
    let mut drives = vec![DriveEntry {
        name: "System".to_string(),
        path: "/".to_string(),
    }];
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for item in read.flatten() {
            let path = item.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if std::fs::canonicalize(&path).is_ok_and(|canon| canon == Path::new("/")) {
                continue;
            }
            drives.push(DriveEntry {
                name: item.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
    DriveListing { drives }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_folders_returns_dirs_sorted_and_marks_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("zed")).unwrap();
        std::fs::create_dir(root.join("Alpha")).unwrap();
        std::fs::create_dir(root.join("repo")).unwrap();
        std::fs::create_dir(root.join("repo/.git")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("notes.txt"), "hi").unwrap();
        let listing = list_folders(&serde_json::json!({
            "path": root.to_string_lossy(),
        }))
        .unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "repo", "zed"]);
        assert!(listing.entries.iter().all(|e| e.is_dir));
        assert!(listing.entries[1].is_repo);
        assert!(!listing.entries[0].is_repo);
        assert!(!listing.truncated);
    }

    #[test]
    fn list_folders_errors_read_like_folder_failures() {
        // The UI distinguishes folder-level failures by the word "folder".
        let error =
            list_folders(&serde_json::json!({ "path": "/definitely/not/here" })).unwrap_err();
        assert!(error.contains("folder"), "unexpected message: {error}");
    }

    #[test]
    fn expand_tilde_resolves_against_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/dev"), format!("{home}/dev"));
        assert_eq!(expand_tilde("/abs"), "/abs");
    }

    #[test]
    fn list_drives_always_offers_the_system_root() {
        let drives = list_drives();
        assert!(drives.drives.iter().any(|d| d.path == "/"));
        // The boot volume's /Volumes alias must not duplicate the System row.
        assert!(
            !drives.drives.iter().any(|d| d.path != "/"
                && std::fs::canonicalize(&d.path).is_ok_and(|p| p == Path::new("/")))
        );
    }
}
