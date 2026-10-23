//! Local-machine surfaces for the add-space palette: folder browsing,
//! mounted volumes, hostname, and the local device row.

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
/// `libc::gethostname` is unix-only; Windows names the machine in
/// COMPUTERNAME, which the system always sets.
fn hostname() -> String {
    #[cfg(unix)]
    {
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
    }
    #[cfg(windows)]
    {
        if let Some(name) = std::env::var("COMPUTERNAME")
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
        {
            return name;
        }
    }
    "This device".to_string()
}

/// The user's home directory. `HOME` is a Unix convention — a plain Windows
/// process has `USERPROFILE` instead (with `HOMEDRIVE`+`HOMEPATH` as the
/// last resort), so resolve all three before giving up.
pub(crate) fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut home = drive;
            home.push(path);
            Some(home)
        })
        .map(|home| home.to_string_lossy().to_string())
        .filter(|home| !home.is_empty())
}

/// The UI expands `~` itself; tolerate it here anyway so the method is
/// callable without the shell's helpers.
pub(crate) fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    let rest = match path.strip_prefix("~/") {
        Some(rest) => Some(rest),
        // A Windows-spelled home path; on unix `~\` is a plain filename.
        None if cfg!(windows) => path.strip_prefix("~\\"),
        None => None,
    };
    if let Some(rest) = rest
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
/// `/`, so it is deduped against the System row. Windows enumerates drive
/// letters (home itself is the rail's separate Home row).
pub(crate) fn list_drives() -> DriveListing {
    #[cfg(windows)]
    {
        windows_drives()
    }
    #[cfg(not(windows))]
    {
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
                if std::fs::canonicalize(&path)
                    .is_ok_and(|canon| canon == std::path::Path::new("/"))
                {
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
}

/// The drive letters a folder browse can descend into: local, removable,
/// and network drives (an empty optical drive is noise, not a destination).
#[cfg(windows)]
fn windows_drives() -> DriveListing {
    // GetDriveTypeW's return codes (winuser.h) — windows-sys 0.52 does not
    // export the constants.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_RAMDISK: u32 = 6;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDriveStringsW};
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer is 256 wchars — the DOS device namespace caps a
    // drive string at 4 — and the returned length is in wchars, NUL included.
    let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if len == 0 || len as usize > buffer.len() {
        return DriveListing { drives: Vec::new() };
    }
    let mut drives = Vec::new();
    for raw in buffer[..len as usize]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
    {
        // SAFETY: each string is NUL-terminated within the returned range.
        let kind = unsafe { GetDriveTypeW(raw.as_ptr()) };
        if !matches!(
            kind,
            DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_REMOTE | DRIVE_RAMDISK
        ) {
            continue;
        }
        let letter = String::from_utf16_lossy(raw);
        drives.push(DriveEntry {
            name: letter.trim_end_matches('\\').to_string(),
            path: letter,
        });
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
        // The module's own resolution chain — HOME, else USERPROFILE — so
        // the test holds wherever home actually comes from.
        let home = home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/dev"), format!("{home}/dev"));
        assert_eq!(expand_tilde("/abs"), "/abs");
    }

    #[test]
    #[cfg(not(windows))]
    fn list_drives_always_offers_the_system_root() {
        let drives = list_drives();
        assert!(drives.drives.iter().any(|d| d.path == "/"));
        // The boot volume's /Volumes alias must not duplicate the System row.
        assert!(!drives.drives.iter().any(|d| d.path != "/"
            && std::fs::canonicalize(&d.path).is_ok_and(|p| p == std::path::Path::new("/"))));
    }

    #[test]
    #[cfg(windows)]
    fn list_drives_offers_the_system_drive() {
        let drives = list_drives();
        assert!(
            drives.drives.iter().any(|d| d.path == "C:\\"),
            "system drive missing: {:?}",
            drives.drives
        );
    }
}
