//! File-sidebar workspace operations: single-level directory listings and
//! bounded text reads, both fenced behind the owning Space's root (ADR-0020
//! groundwork). The UI never reads workspace contents itself; every boundary
//! rule — root containment, `.git` exclusion, symlink traversal — is enforced
//! here, engine-side, before any filesystem work.

use std::cmp::Ordering;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use holt_proto::{
    WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileRead, WorkspaceLineEndings, WorkspaceListing,
};

/// The largest text file the editor will open (2 MiB). Bigger files (and any
/// non-UTF-8 or binary content) come back as an unsupported reason instead of
/// a lossy decode.
pub(crate) const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

/// Per-directory listing cap. Rows are virtualized client-side, but a runaway
/// directory (generated trees, caches) is still bounded per request.
const ENTRY_CAP: usize = 10_000;

/// A sniffing budget for binary detection: a NUL byte this early in a text
/// file means binary, not prose.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Why a workspace request failed — the RPC layer maps each fault to a
/// message the UI can render as a distinct state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FilesFault {
    #[error("{0} is outside this space's working directory")]
    OutsideRoot(String),
    #[error("{0} is excluded (.git)")]
    GitExcluded(String),
    #[error("{0} does not exist")]
    NotFound(String),
    #[error("{0} is a directory")]
    IsDirectory(String),
    #[error("{0} is not a directory")]
    NotDirectory(String),
    #[error("{0}")]
    BadName(String),
    #[error("{0} already exists")]
    Collision(String),
    #[error("{0}")]
    InvalidDestination(String),
    #[error("could not read {0}: {1}")]
    Io(String, String),
}

/// Canonicalize `path`, tolerating a missing final component (the leaf may
/// not exist yet): canonicalize the parent and re-append the leaf name.
fn canonicalize_lenient(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no parent",
        ));
    };
    let parent_canonical = parent.canonicalize()?;
    match path.file_name() {
        Some(name) => Ok(parent_canonical.join(name)),
        None => Ok(parent_canonical),
    }
}

/// Resolve a requested path against `root` and enforce the sidebar's
/// boundaries. Accepts absolute paths (validated against the canonical root)
/// and root-relative ones; `~` expands first. Returns the canonical path —
/// symlinks resolved — so containment is judged on where the path actually
/// lands, not on the spelling the caller sent.
pub(crate) fn resolve_inside_root(root: &Path, requested: &str) -> Result<PathBuf, FilesFault> {
    let expanded = crate::local_fs::expand_tilde(requested);
    let candidate = Path::new(&expanded);
    let joined: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let display = joined.display().to_string();
    // A missing leaf is legal for the engine's future create/rename flows,
    // but listing/reading still requires an existing entry downstream.
    let canonical = match canonicalize_lenient(&joined) {
        Ok(canonical) => canonical,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => FilesFault::NotFound(display),
                _ => FilesFault::Io(display, error.to_string()),
            });
        }
    };
    let canonical_root = root
        .canonicalize()
        .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(FilesFault::OutsideRoot(display));
    }
    exclude_git(&canonical, &canonical_root)?;
    Ok(canonical)
}

/// `.git` never appears in the tree, in any form — directory or the
/// gitdir-pointer file a linked worktree carries at its root.
fn exclude_git(canonical: &Path, canonical_root: &Path) -> Result<(), FilesFault> {
    let relative = canonical
        .strip_prefix(canonical_root)
        .unwrap_or_else(|_| Path::new(""));
    for component in relative.components() {
        if let Component::Normal(part) = component
            && part == ".git"
        {
            return Err(FilesFault::GitExcluded(canonical.display().to_string()));
        }
    }
    Ok(())
}

/// Classify one directory entry's kind, resolving symlinks with the root's
/// containment rule: inside-root aliases stay traversable, outside-root and
/// broken links become dead-end rows. `canonical_root` is resolved once by
/// the caller — a big directory with many symlinks must not re-canonicalize
/// the root per row.
fn classify_entry(canonical_root: &Path, path: &Path) -> WorkspaceEntryKind {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        // Racing delete: read_dir gave us the name, the entry is gone.
        return WorkspaceEntryKind::SymlinkBroken;
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        // Resolve it fully. `canonicalize` follows the whole chain, so
        // cycles fail (natural traversal guard) and the landing path is
        // what containment is judged on.
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) => return WorkspaceEntryKind::SymlinkBroken,
        };
        let target_is_dir = resolved.is_dir();
        let inside =
            resolved.starts_with(canonical_root) && exclude_git(&resolved, canonical_root).is_ok();
        if inside {
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir,
                resolved_path: resolved.display().to_string(),
            }
        } else {
            WorkspaceEntryKind::SymlinkOutside { target_is_dir }
        }
    } else if file_type.is_dir() {
        WorkspaceEntryKind::Directory
    } else {
        WorkspaceEntryKind::File
    }
}

/// Directories first, then files, names compared case-insensitively with the
/// original spelling as the deterministic tiebreaker.
fn entry_order(a: &WorkspaceEntry, b: &WorkspaceEntry) -> Ordering {
    let dir = |entry: &WorkspaceEntry| {
        matches!(
            entry.kind,
            WorkspaceEntryKind::Directory
                | WorkspaceEntryKind::SymlinkInside {
                    target_is_dir: true,
                    ..
                }
                | WorkspaceEntryKind::SymlinkOutside {
                    target_is_dir: true
                }
        )
    };
    match (dir(a), dir(b)) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name)),
    }
}

/// One level of `read_dir`, engine-side. `requested` may be empty to list the
/// root itself.
pub(crate) fn list_directory(root: &Path, requested: &str) -> Result<WorkspaceListing, FilesFault> {
    let canonical = if requested.trim().is_empty() {
        root.canonicalize()
            .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?
    } else {
        resolve_inside_root(root, requested)?
    };
    let metadata = std::fs::metadata(&canonical).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => FilesFault::NotFound(canonical.display().to_string()),
        _ => FilesFault::Io(canonical.display().to_string(), error.to_string()),
    })?;
    if !metadata.is_dir() {
        return Err(FilesFault::NotDirectory(canonical.display().to_string()));
    }
    let read = std::fs::read_dir(&canonical)
        .map_err(|error| FilesFault::Io(canonical.display().to_string(), error.to_string()))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().to_string();
        // `.git` is invisible in any form (directory or gitdir pointer).
        if name == ".git" {
            continue;
        }
        let path = item.path();
        let kind = classify_entry(&canonical_root, &path);
        let size = match &kind {
            WorkspaceEntryKind::File => {
                std::fs::symlink_metadata(&path).ok().map(|meta| meta.len())
            }
            _ => None,
        };
        if entries.len() >= ENTRY_CAP {
            truncated = true;
            break;
        }
        entries.push(WorkspaceEntry {
            name,
            path: path.display().to_string(),
            kind,
            size,
        });
    }
    entries.sort_by(entry_order);
    Ok(WorkspaceListing {
        path: canonical.display().to_string(),
        entries,
        truncated,
    })
}

/// Detect a file's line-ending shape without rewriting anything.
fn line_endings(bytes: &[u8]) -> WorkspaceLineEndings {
    let mut lf = false;
    let mut crlf = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf = true;
                index += 2;
            }
            b'\n' => {
                lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    match (lf, crlf) {
        (true, true) => WorkspaceLineEndings::Mixed,
        (true, false) => WorkspaceLineEndings::Lf,
        (false, true) => WorkspaceLineEndings::Crlf,
        (false, false) => WorkspaceLineEndings::None,
    }
}

/// Opaque disk version token (length + mtime) — the save flow's stale-check
/// baseline. A content change that leaves both untouched is out of scope for
/// stat-based versioning; ticket 02 layers the recheck on this token.
fn version_token(metadata: &std::fs::Metadata) -> String {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}:{}", metadata.len(), mtime)
}

/// Read a text file for the editor: bounded, UTF-8-strict, with the source
/// facts (BOM, line endings, version token) a save must reproduce.
pub(crate) fn read_file(root: &Path, requested: &str) -> Result<WorkspaceFileRead, FilesFault> {
    let canonical = resolve_inside_root(root, requested)?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => FilesFault::NotFound(canonical.display().to_string()),
        _ => FilesFault::Io(canonical.display().to_string(), error.to_string()),
    })?;
    if metadata.is_dir() {
        return Err(FilesFault::IsDirectory(canonical.display().to_string()));
    }
    let version = version_token(&metadata);
    let bytes_len = metadata.len();
    if bytes_len > MAX_TEXT_BYTES {
        return Ok(WorkspaceFileRead {
            path: canonical.display().to_string(),
            version,
            bytes: bytes_len,
            bom: false,
            line_endings: WorkspaceLineEndings::Lf,
            text: None,
            unsupported_reason: Some(format!(
                "This file is {} MiB; the editor supports text up to 2 MiB.",
                bytes_len / (1024 * 1024)
            )),
        });
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|error| FilesFault::Io(canonical.display().to_string(), error.to_string()))?;
    let bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let bytes_len = bytes.len() as u64;
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Ok(WorkspaceFileRead {
            path: canonical.display().to_string(),
            version,
            bytes: bytes_len,
            bom: false,
            line_endings: WorkspaceLineEndings::Lf,
            text: None,
            unsupported_reason: Some("This file is binary; the editor only opens text.".into()),
        });
    }
    // The BOM never enters the editable buffer — the flag carries it and the
    // save reattaches it. A double-encoded mark on disk is preserved as
    // content only when it is not the leading one.
    let stripped = if bom { bytes[3..].to_vec() } else { bytes };
    let text = match String::from_utf8(stripped) {
        Ok(text) => text,
        Err(_) => {
            return Ok(WorkspaceFileRead {
                path: canonical.display().to_string(),
                version,
                bytes: bytes_len,
                bom: false,
                line_endings: WorkspaceLineEndings::Lf,
                text: None,
                unsupported_reason: Some(
                    "This file is not valid UTF-8; the editor only opens UTF-8 text.".into(),
                ),
            });
        }
    };
    let endings = line_endings(text.as_bytes());
    Ok(WorkspaceFileRead {
        path: canonical.display().to_string(),
        version,
        bytes: bytes_len,
        bom,
        line_endings: endings,
        text: Some(text),
        unsupported_reason: None,
    })
}

/// Save an edited text file: validate the disk version the draft was based
/// on, write atomically (same-directory temp + rename, permissions
/// preserved), and recheck before the rename so a change that lands mid-save
/// fails closed instead of overwriting. The text is written verbatim — the
/// buffer preserved the original line endings — with the original BOM
/// reattached when `bom` is set.
pub(crate) fn save_file(
    root: &Path,
    requested: &str,
    text: &str,
    expected_version: &str,
    expect_disk_version: Option<&str>,
    bom: bool,
) -> Result<holt_proto::WorkspaceFileSave, FilesFault> {
    let canonical = resolve_inside_root(root, requested)?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => FilesFault::NotFound(canonical.display().to_string()),
        _ => FilesFault::Io(canonical.display().to_string(), error.to_string()),
    })?;
    if metadata.is_dir() {
        return Err(FilesFault::IsDirectory(canonical.display().to_string()));
    }
    // A missing file must not be silently recreated by an ordinary save.
    if !canonical.exists() {
        return Err(FilesFault::NotFound(canonical.display().to_string()));
    }
    // The version the save must still match: the read baseline, or — for an
    // explicitly confirmed overwrite — the disk version the user reviewed.
    // Any intervening change misses the token and re-conflicts.
    let baseline = expect_disk_version.unwrap_or(expected_version);
    if version_token(&metadata) != baseline {
        return Ok(holt_proto::WorkspaceFileSave {
            status: holt_proto::WorkspaceSaveStatus::VersionConflict,
            version: None,
            disk_version: Some(version_token(&metadata)),
        });
    }
    let permissions = metadata.permissions();
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(text.as_bytes());
    let temp = canonical.with_extension(format!("holt-save-{}", std::process::id()));
    // Clean up the temp file on every failure path — a stray `.holt-save-*`
    // next to the file would show up in the tree.
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write {
        let _ = std::fs::remove_file(&temp);
        return Err(FilesFault::Io(
            canonical.display().to_string(),
            error.to_string(),
        ));
    }
    std::fs::set_permissions(&temp, permissions).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        FilesFault::Io(canonical.display().to_string(), error.to_string())
    })?;
    // Recheck right before the rename: a change landing between the first
    // check and the write still fails closed.
    let recheck = std::fs::symlink_metadata(&canonical);
    let conflict = match &recheck {
        Ok(meta) => version_token(meta) != baseline,
        Err(_) => true,
    };
    if conflict {
        let _ = std::fs::remove_file(&temp);
        let disk_version = recheck.ok().map(|meta| version_token(&meta));
        return Ok(holt_proto::WorkspaceFileSave {
            status: holt_proto::WorkspaceSaveStatus::VersionConflict,
            version: None,
            disk_version,
        });
    }
    std::fs::rename(&temp, &canonical).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        FilesFault::Io(canonical.display().to_string(), error.to_string())
    })?;
    let saved = std::fs::symlink_metadata(&canonical)
        .map_err(|error| FilesFault::Io(canonical.display().to_string(), error.to_string()))?;
    Ok(holt_proto::WorkspaceFileSave {
        status: holt_proto::WorkspaceSaveStatus::Saved,
        version: Some(version_token(&saved)),
        disk_version: None,
    })
}

/// Save As: write `text` to a NEW path inside the root. The destination
/// must not exist (no overwrite as a side effect), its parent must, and
/// the original file is never touched.
pub(crate) fn write_file_as(
    root: &Path,
    requested: &str,
    text: &str,
    bom: bool,
) -> Result<holt_proto::WorkspaceFileSave, FilesFault> {
    let canonical = resolve_inside_root(root, requested)?;
    // The destination's parent must exist and be a directory — new-file
    // destinations validate through their existing parents.
    let parent = canonical
        .parent()
        .ok_or_else(|| FilesFault::NotFound(canonical.display().to_string()))?;
    let parent_metadata = std::fs::metadata(parent).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => FilesFault::NotFound(parent.display().to_string()),
        _ => FilesFault::Io(parent.display().to_string(), error.to_string()),
    })?;
    if !parent_metadata.is_dir() {
        return Err(FilesFault::NotDirectory(parent.display().to_string()));
    }
    if canonical.exists() {
        return Err(FilesFault::Io(
            canonical.display().to_string(),
            "the destination already exists".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(text.as_bytes());
    static SAVE_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = SAVE_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let temp = canonical.with_file_name(format!(".{file_name}.holt-as-{unique}"));
    let write = (|| -> std::io::Result<()> {
        // create_new refuses to clobber a race-created destination.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write {
        let _ = std::fs::remove_file(&temp);
        return Err(FilesFault::Io(
            canonical.display().to_string(),
            error.to_string(),
        ));
    }
    std::fs::rename(&temp, &canonical).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        FilesFault::Io(canonical.display().to_string(), error.to_string())
    })?;
    let saved = std::fs::symlink_metadata(&canonical)
        .map_err(|error| FilesFault::Io(canonical.display().to_string(), error.to_string()))?;
    Ok(holt_proto::WorkspaceFileSave {
        status: holt_proto::WorkspaceSaveStatus::Saved,
        version: Some(version_token(&saved)),
        disk_version: None,
    })
}

/// A legal entry name: non-empty, no separators, not a `.`/`..`/`.git`
/// traversal, no NUL. Unicode and spaces are welcome.
fn valid_entry_name(name: &str) -> Result<(), FilesFault> {
    if name.trim().is_empty() {
        return Err(FilesFault::BadName("the name must not be empty".into()));
    }
    if name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name == ".git"
        || name.contains('\0')
    {
        return Err(FilesFault::BadName(format!("{name:?} is not a valid name")));
    }
    Ok(())
}

/// Create a file or directory under an EXISTING parent inside the root
/// (ticket 06). Collisions refuse; nothing is overwritten; `.git` and
/// root escapes never land.
pub(crate) fn create_entry(
    root: &Path,
    parent: &str,
    name: &str,
    is_dir: bool,
) -> Result<(), FilesFault> {
    valid_entry_name(name)?;
    let parent_canonical = if parent.trim().is_empty() {
        root.canonicalize()
            .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?
    } else {
        resolve_inside_root(root, parent)?
    };
    let parent_metadata =
        std::fs::symlink_metadata(&parent_canonical).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                FilesFault::NotFound(parent_canonical.display().to_string())
            }
            _ => FilesFault::Io(parent_canonical.display().to_string(), error.to_string()),
        })?;
    if parent_metadata.is_symlink() {
        // Creating through a symlinked parent is fine only when its target
        // stays inside the root — resolve_inside_root already fenced it.
    }
    if !parent_canonical.is_dir() {
        return Err(FilesFault::NotDirectory(
            parent_canonical.display().to_string(),
        ));
    }
    let destination = parent_canonical.join(name);
    if destination.exists() || std::fs::symlink_metadata(&destination).is_ok() {
        return Err(FilesFault::Collision(destination.display().to_string()));
    }
    let created = if is_dir {
        std::fs::create_dir(&destination)
    } else {
        // create_new refuses to touch an existing name — a racing creator
        // can never be clobbered as a side effect.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map(|_| ())
    };
    created
        .map_err(|error| FilesFault::Io(destination.display().to_string(), error.to_string()))?;
    Ok(())
}

/// Resolve an ENTRY address inside the root WITHOUT following the final
/// component: management operations (rename, move, delete) act on the
/// entry itself — a symlink renames as a symlink, never as its target.
/// Symlinks in the DIRECTORY part still resolve (and must stay in root).
fn resolve_entry_inside_root(root: &Path, requested: &str) -> Result<PathBuf, FilesFault> {
    let expanded = crate::local_fs::expand_tilde(requested);
    let candidate = Path::new(&expanded);
    let joined: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let display = joined.display().to_string();
    let Some(leaf) = joined
        .file_name()
        .map(|leaf| leaf.to_string_lossy().to_string())
    else {
        return Err(FilesFault::BadName(display));
    };
    if leaf == ".git" {
        return Err(FilesFault::GitExcluded(display));
    }
    let parent = joined.parent().unwrap_or_else(|| Path::new(""));
    let parent_canonical = parent
        .canonicalize()
        .map_err(|_| FilesFault::NotFound(joined.display().to_string()))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?;
    if !parent_canonical.starts_with(&canonical_root) {
        return Err(FilesFault::OutsideRoot(display));
    }
    exclude_git(&parent_canonical, &canonical_root)?;
    Ok(parent_canonical.join(leaf))
}

/// Same file, different letter case (`a.md` vs `A.md`) in the same
/// directory — the case-insensitive-filesystem rename the exists-check
/// cannot classify as a collision because stat sees the source itself.
fn same_entry_case_insensitive(source: &Path, destination: &Path) -> bool {
    let (Some(from_parent), Some(to_parent)) = (source.parent(), destination.parent()) else {
        return false;
    };
    let (Some(from_name), Some(to_name)) = (
        source.file_name().and_then(|n| n.to_str()),
        destination.file_name().and_then(|n| n.to_str()),
    ) else {
        return false;
    };
    from_parent == to_parent && from_name.eq_ignore_ascii_case(to_name) && from_name != to_name
}

/// Rename refusing to replace an existing destination. macOS offers
/// `renamex_np(RENAME_EXCL)`; elsewhere the pre-checked std rename stands
/// (its residual race is documented, not widened).
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        const RENAME_EXCL: u64 = 0x0000_0004;
        // SAFETY: renamex_np takes NUL-terminated paths and returns -1 with
        // errno on failure; the CStrings outlive the call. Declared locally
        // because the libc crate does not export this Darwin entry point.
        #[allow(improper_ctypes)]
        unsafe extern "C" {
            fn renamex_np(
                from: *const std::os::raw::c_char,
                to: *const std::os::raw::c_char,
                flags: u64,
            ) -> std::os::raw::c_int;
        }
        let from = CString::new(source.as_os_str().as_bytes())?;
        let to = CString::new(destination.as_os_str().as_bytes())?;
        let rc = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_EXCL) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::rename(source, destination)
    }
}

/// Rename an entry IN PLACE (same directory, new name — ticket 06; cross-
/// directory moves are ticket 07). `fs::rename` renames a symlink entry
/// itself, never its target. Collisions refuse; drafts are the UI's to
/// carry. On failure the original identity is untouched.
pub(crate) fn rename_entry(root: &Path, from: &str, new_name: &str) -> Result<String, FilesFault> {
    valid_entry_name(new_name)?;
    let source = resolve_entry_inside_root(root, from)?;
    // The source must exist under its own name (a broken symlink still
    // renames — it is an entry).
    if std::fs::symlink_metadata(&source).is_err() {
        return Err(FilesFault::NotFound(source.display().to_string()));
    }
    let destination = source
        .parent()
        .and_then(|parent| {
            std::fs::canonicalize(parent)
                .ok()
                .map(|parent| parent.join(new_name))
        })
        .ok_or_else(|| FilesFault::NotFound(source.display().to_string()))?;
    // Case-insensitive filesystems stat the source itself for a case-only
    // rename (`readme` → `README`): the destination exists check cannot see
    // a third party there, and the rename is inherently collision-free.
    let case_only = same_entry_case_insensitive(&source, &destination);
    if !case_only
        && destination != source
        && (destination.exists() || std::fs::symlink_metadata(&destination).is_ok())
    {
        return Err(FilesFault::Collision(destination.display().to_string()));
    }
    // Noreplace semantics where the platform offers them, so a racing
    // creator at the destination can never be silently replaced.
    if case_only {
        std::fs::rename(&source, &destination)
    } else {
        rename_noreplace(&source, &destination)
    }
    .map_err(|error| FilesFault::Io(source.display().to_string(), error.to_string()))?;
    Ok(destination.display().to_string())
}

/// Move an entry into an EXISTING directory inside the root (ticket 07's
/// cut/paste). The entry itself moves — a symlink travels as a link and its
/// target is untouched. The source, the destination directory, and the
/// resulting path are all revalidated here: outside-root, `.git`, missing
/// source, collision, pasting into the entry's own subtree, and a no-op
/// paste into the current directory all refuse. There is no copy/delete
/// fallback: one `rename_noreplace`, and an `Io` fault (including
/// cross-device EXDEV) leaves the source exactly where it was.
pub(crate) fn move_entry(
    root: &Path,
    from: &str,
    destination_dir: &str,
) -> Result<String, FilesFault> {
    let source = resolve_entry_inside_root(root, from)?;
    if std::fs::symlink_metadata(&source).is_err() {
        return Err(FilesFault::NotFound(source.display().to_string()));
    }
    let destination_directory = if destination_dir.trim().is_empty() {
        root.canonicalize()
            .map_err(|error| FilesFault::Io(root.display().to_string(), error.to_string()))?
    } else {
        let dir = resolve_inside_root(root, destination_dir)?;
        if !dir.is_dir() {
            return Err(FilesFault::NotDirectory(dir.display().to_string()));
        }
        dir
    };
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| FilesFault::BadName(source.display().to_string()))?;
    let destination = destination_directory.join(&name);
    if destination == source {
        return Err(FilesFault::InvalidDestination(format!(
            "{} is already in that directory",
            source.display()
        )));
    }
    // Pasting a directory into itself or one of its own descendants would
    // swallow its own subtree. The equal case already refused above, so a
    // component-wise `starts_with` here means a strict descendant.
    if destination.starts_with(&source) {
        return Err(FilesFault::InvalidDestination(format!(
            "cannot move {} into itself or its own descendant",
            source.display()
        )));
    }
    if destination.exists() || std::fs::symlink_metadata(&destination).is_ok() {
        return Err(FilesFault::Collision(destination.display().to_string()));
    }
    rename_noreplace(&source, &destination)
        .map_err(|error| FilesFault::Io(source.display().to_string(), error.to_string()))?;
    Ok(destination.display().to_string())
}

/// Move an entry to the operating system's trash (ticket 07). The ENTRY
/// goes — a symlink is trashed as a link, never its target — and a trash
/// that is unavailable or refuses reports the failure. There is no
/// permanent-delete fallback: the entry is either in the trash or still in
/// place.
pub(crate) fn trash_entry(root: &Path, from: &str) -> Result<(), FilesFault> {
    let entry = resolve_entry_inside_root(root, from)?;
    if std::fs::symlink_metadata(&entry).is_err() {
        return Err(FilesFault::NotFound(entry.display().to_string()));
    }
    crate::trash::move_to_trash(&entry)
        .map_err(|message| FilesFault::Io(entry.display().to_string(), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn listings_sort_directories_first_and_skip_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("zed")).unwrap();
        std::fs::create_dir_all(root.join("Alpha")).unwrap();
        write(&root.join("notes.txt"), b"hi");
        write(&root.join(".hidden"), b"secret");
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        write(&root.join(".git/HEAD"), b"ref");
        let listing = list_directory(root, "").unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        // Directories first, then files case-insensitively.
        assert_eq!(names, ["Alpha", "zed", ".hidden", "notes.txt"]);
        assert!(matches!(
            listing.entries[0].kind,
            WorkspaceEntryKind::Directory
        ));
        assert!(matches!(listing.entries[2].kind, WorkspaceEntryKind::File));
    }

    #[test]
    fn gitdir_pointer_files_are_excluded_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("worktree/.git"), b"gitdir: /elsewhere");
        write(&root.join("worktree/src.rs"), b"// x");
        let listing = list_directory(&root.join("worktree"), "").unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["src.rs"]);
        // And it cannot be listed/read through the RPC guard either.
        let fault = list_directory(root, "worktree/.git").unwrap_err();
        assert!(matches!(fault, FilesFault::GitExcluded(_)));
    }

    #[test]
    fn symlinks_classify_against_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("real/file.txt"), b"x");
        std::os::unix::fs::symlink(root.join("real"), root.join("inside")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("../outside-does-not-matter"),
            root.join("broken"),
        )
        .unwrap();
        let listing = list_directory(root, "").unwrap();
        let inside = listing.entries.iter().find(|e| e.name == "inside").unwrap();
        match &inside.kind {
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir,
                resolved_path,
            } => {
                assert!(target_is_dir);
                assert!(resolved_path.ends_with("real"));
            }
            other => panic!("inside should be SymlinkInside, got {other:?}"),
        }
        let broken = listing.entries.iter().find(|e| e.name == "broken").unwrap();
        assert!(matches!(broken.kind, WorkspaceEntryKind::SymlinkBroken));
        // Listing THROUGH the inside alias works and stays fenced.
        let aliased = list_directory(root, "inside").unwrap();
        assert_eq!(aliased.path, root.join("real").canonicalize().unwrap());
        assert_eq!(aliased.entries.len(), 1);
    }

    #[test]
    fn outside_root_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), b"x");
        std::os::unix::fs::symlink(outside.path(), root.join("elsewhere")).unwrap();
        // Direct absolute escape.
        let outside_path = outside.path().display().to_string();
        let fault = list_directory(root, &outside_path).unwrap_err();
        assert!(matches!(fault, FilesFault::OutsideRoot(_)));
        // Escape through a symlinked directory.
        let fault = list_directory(root, "elsewhere").unwrap_err();
        assert!(matches!(fault, FilesFault::OutsideRoot(_)));
        let fault = read_file(root, "elsewhere/secret.txt").unwrap_err();
        assert!(matches!(fault, FilesFault::OutsideRoot(_)));
        // Dot-dot traversal canonicalizes away: the parent of the root is
        // outside it.
        let fault = list_directory(root, "..").unwrap_err();
        assert!(matches!(fault, FilesFault::OutsideRoot(_)));
    }

    #[test]
    fn reads_preserve_bom_and_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("bom.txt"),
            "\u{FEFF}hello\r\nworld\r\n".as_bytes(),
        );
        let read = read_file(root, "bom.txt").unwrap();
        assert_eq!(read.text.as_deref(), Some("hello\r\nworld\r\n"));
        assert!(read.bom);
        assert_eq!(read.line_endings, WorkspaceLineEndings::Crlf);

        write(&root.join("mixed.txt"), b"a\nb\r\nc\n");
        let read = read_file(root, "mixed.txt").unwrap();
        assert_eq!(read.line_endings, WorkspaceLineEndings::Mixed);

        write(&root.join("none.txt"), b"single line");
        let read = read_file(root, "none.txt").unwrap();
        assert_eq!(read.line_endings, WorkspaceLineEndings::None);
    }

    #[test]
    fn unsupported_files_report_reasons_instead_of_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("image.bin"), b"\x89PNG\r\n\x1a\n\x00\x00\x00");
        let read = read_file(root, "image.bin").unwrap();
        assert_eq!(read.text, None);
        assert!(
            read.unsupported_reason
                .as_deref()
                .unwrap()
                .contains("binary")
        );

        write(&root.join("latin1.txt"), b"caf\xe9");
        let read = read_file(root, "latin1.txt").unwrap();
        assert_eq!(read.text, None);
        assert!(
            read.unsupported_reason
                .as_deref()
                .unwrap()
                .contains("UTF-8")
        );

        let huge: Vec<u8> = vec![b'x'; (MAX_TEXT_BYTES + 1) as usize];
        write(&root.join("huge.txt"), &huge);
        let read = read_file(root, "huge.txt").unwrap();
        assert_eq!(read.text, None);
        assert!(
            read.unsupported_reason
                .as_deref()
                .unwrap()
                .contains("2 MiB")
        );
    }

    #[test]
    fn reads_go_through_inside_aliases_and_read_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("real/notes.md"), b"# hi\n");
        std::os::unix::fs::symlink(root.join("real/notes.md"), root.join("alias.md")).unwrap();
        let read = read_file(root, "alias.md").unwrap();
        assert_eq!(read.text.as_deref(), Some("# hi\n"));
        assert_eq!(
            read.path,
            root.join("real/notes.md")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[test]
    fn directories_refuse_as_files_and_missing_paths_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let fault = read_file(root, "sub").unwrap_err();
        assert!(matches!(fault, FilesFault::IsDirectory(_)));
        let fault = read_file(root, "nope.txt").unwrap_err();
        assert!(matches!(fault, FilesFault::NotFound(_)));
        let fault = list_directory(root, "nope").unwrap_err();
        assert!(matches!(fault, FilesFault::NotFound(_)));
    }

    #[test]
    fn cyclic_symlink_chains_fail_closed_not_forever() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(root.join("b"), root.join("a")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("b")).unwrap();
        let listing = list_directory(root, "").unwrap();
        // The rows exist; both land broken because the chain never resolves.
        for name in ["a", "b"] {
            let entry = listing.entries.iter().find(|e| e.name == name).unwrap();
            assert!(matches!(entry.kind, WorkspaceEntryKind::SymlinkBroken));
        }
        // Reading through the chain fails closed (ELOOP surfaces as an io
        // fault, never as content).
        let fault = read_file(root, "a").unwrap_err();
        assert!(matches!(fault, FilesFault::Io(_, _)));
    }

    #[test]
    fn truncation_flags_runaway_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("many");
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..(ENTRY_CAP + 5) {
            std::fs::write(root.join(format!("f{index:05}.txt")), b"x").unwrap();
        }
        let listing = list_directory(dir.path(), "many").unwrap();
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), ENTRY_CAP);
    }
}
