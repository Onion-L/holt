//! Turn restore: write a settled Turn's pre-Turn content back from its
//! persisted record (`turn_change_store`). Plain file I/O, no Git — the
//! record is a content snapshot, so a restore is "put `old_text` back",
//! gated per file on the disk still hashing to the record's
//! `new_content_hash`. A file that moved on since settlement is refused,
//! never merged.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use holt_proto::{
    TurnFileChange, TurnFileChangeStatus, TurnRestoreFile, TurnRestoreOutcome, TurnRestoreOverlap,
    TurnRestoreRefusal, TurnRestoreReply,
};

use crate::git::{TurnFileContent, sha256_hex};
use crate::turn_change_store::TurnChangeRecord;

/// Restore `paths` of `record` (empty = every file) under `root`, the Git
/// work tree the record's repo-relative paths resolve against. `later` are
/// the chat's other records; the ones settled after `record` feed the
/// overlap report. With `dry_run` nothing is written.
pub(crate) fn restore(
    root: &Path,
    record: &TurnChangeRecord,
    later: &[TurnChangeRecord],
    paths: &[String],
    dry_run: bool,
) -> Result<TurnRestoreReply, String> {
    let mut changes: Vec<&TurnFileChange> = Vec::new();
    if paths.is_empty() {
        changes.extend(record.files.iter());
    } else {
        for path in paths {
            let change = record
                .files
                .iter()
                .find(|file| &file.path == path)
                .ok_or_else(|| format!("{path} is not part of that turn's changes"))?;
            changes.push(change);
        }
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", root.display()))?;

    let touched: BTreeSet<&str> = changes
        .iter()
        .flat_map(|change| [Some(change.path.as_str()), change.old_path.as_deref()])
        .flatten()
        .collect();
    let mut later: Vec<&TurnChangeRecord> = later
        .iter()
        .filter(|other| {
            other.message_id != record.message_id && other.settled_at > record.settled_at
        })
        .collect();
    later.sort_by_key(|other| other.settled_at);
    let mut overlaps = Vec::new();
    let mut overlapped: BTreeSet<&str> = BTreeSet::new();
    for other in later {
        let shared: BTreeSet<&str> = other
            .files
            .iter()
            .flat_map(|file| [Some(file.path.as_str()), file.old_path.as_deref()])
            .flatten()
            .filter(|path| touched.contains(path))
            .collect();
        if shared.is_empty() {
            continue;
        }
        overlapped.extend(shared.iter().copied());
        overlaps.push(TurnRestoreOverlap {
            message_id: other.message_id.clone(),
            paths: shared.into_iter().map(str::to_string).collect(),
        });
    }

    let files = changes
        .into_iter()
        .map(|change| {
            let is_overlapped = overlapped.contains(change.path.as_str())
                || change
                    .old_path
                    .as_deref()
                    .is_some_and(|path| overlapped.contains(path));
            let outcome = match record.content_for(&change.path) {
                Some(content) => restore_file(&root, change, content, is_overlapped, dry_run),
                None => refused(TurnRestoreRefusal::Io {
                    message: "no stored content for this file".into(),
                }),
            };
            TurnRestoreFile {
                path: change.path.clone(),
                outcome,
            }
        })
        .collect();
    Ok(TurnRestoreReply { files, overlaps })
}

fn refused(reason: TurnRestoreRefusal) -> TurnRestoreOutcome {
    TurnRestoreOutcome::Refused { reason }
}

fn restore_file(
    root: &Path,
    change: &TurnFileChange,
    content: &TurnFileContent,
    overlapped: bool,
    dry_run: bool,
) -> TurnRestoreOutcome {
    if content.truncated {
        return refused(TurnRestoreRefusal::Truncated);
    }
    if content.binary || change.binary {
        return refused(TurnRestoreRefusal::Binary);
    }
    // The stored text is lossy for non-UTF-8 sources; writing it back
    // would corrupt the file, so it must hash to the original bytes.
    let text_matches_hash = match (&content.old_text, &content.old_content_hash) {
        (Some(text), Some(hash)) => &sha256_hex(text.as_bytes()) == hash,
        (None, None) => true,
        _ => false,
    };
    if !text_matches_hash {
        return refused(TurnRestoreRefusal::LossyText);
    }

    let Some(dest) = safe_target(root, &content.path) else {
        return refused(TurnRestoreRefusal::UnsafePath);
    };
    let source = match (change.status, change.old_path.as_deref()) {
        (TurnFileChangeStatus::Renamed, Some(old_path)) => match safe_target(root, old_path) {
            Some(path) => Some(path),
            None => return refused(TurnRestoreRefusal::UnsafePath),
        },
        _ => None,
    };

    let current = |path: &Path| -> Result<Option<String>, TurnRestoreOutcome> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(sha256_hex(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(refused(TurnRestoreRefusal::Io {
                message: format!("{}: {error}", path.display()),
            })),
        }
    };
    let dest_hash = match current(&dest) {
        Ok(hash) => hash,
        Err(outcome) => return outcome,
    };
    let old_hash = &content.old_content_hash;
    let new_hash = &content.new_content_hash;

    let (already, writable) = match &source {
        // A rename restores to: the old path holds the old content and the
        // new path is gone.
        Some(source) => {
            let source_hash = match current(source) {
                Ok(hash) => hash,
                Err(outcome) => return outcome,
            };
            (
                dest_hash.is_none() && &source_hash == old_hash,
                &dest_hash == new_hash && (source_hash.is_none() || &source_hash == old_hash),
            )
        }
        None => (&dest_hash == old_hash, &dest_hash == new_hash),
    };
    if already {
        return TurnRestoreOutcome::AlreadyRestored;
    }
    if !writable {
        return refused(if overlapped {
            TurnRestoreRefusal::LaterTurn
        } else {
            TurnRestoreRefusal::Conflict
        });
    }
    if dry_run {
        return TurnRestoreOutcome::Restored;
    }

    let result = match (&source, &content.old_text) {
        (Some(source), Some(text)) => {
            write_atomic(source, text).and_then(|()| std::fs::remove_file(&dest))
        }
        (None, Some(text)) => write_atomic(&dest, text),
        (_, None) => match std::fs::remove_file(&dest) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    };
    match result {
        Ok(()) => TurnRestoreOutcome::Restored,
        Err(error) => refused(TurnRestoreRefusal::Io {
            message: error.to_string(),
        }),
    }
}

/// `root` joined with a record path, or `None` when the path could leave
/// the work tree: non-normal components, `.git`, a symlinked target, or an
/// existing ancestor that canonicalizes outside the root.
fn safe_target(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) if part != ".git" => clean.push(part),
            _ => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        return None;
    }
    let target = root.join(&clean);
    if std::fs::symlink_metadata(&target).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return None;
    }
    let ancestor = target.ancestors().find(|path| path.exists())?;
    ancestor
        .canonicalize()
        .ok()?
        .starts_with(root)
        .then_some(target)
}

/// Write via a sibling temp file + rename so a crash never leaves a
/// half-written file, keeping the target's permissions when it exists.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path.parent().expect("target has a parent");
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = parent.join(format!(".{name}.holt-restore-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, meta.permissions())?;
        }
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn hash(text: &str) -> Option<String> {
        Some(sha256_hex(text.as_bytes()))
    }

    fn change(path: &str, status: TurnFileChangeStatus, old_path: Option<&str>) -> TurnFileChange {
        TurnFileChange {
            path: path.into(),
            old_path: old_path.map(str::to_string),
            status,
            additions: 1,
            deletions: 1,
            binary: false,
        }
    }

    fn content(path: &str, old: Option<&str>, new: Option<&str>) -> TurnFileContent {
        TurnFileContent {
            path: path.into(),
            old_text: old.map(str::to_string),
            new_text: new.map(str::to_string),
            old_content_hash: old.and_then(hash),
            new_content_hash: new.and_then(hash),
            binary: false,
            truncated: false,
        }
    }

    fn record(message_id: &str, files: Vec<(TurnFileChange, TurnFileContent)>) -> TurnChangeRecord {
        TurnChangeRecord {
            message_id: message_id.into(),
            cwd: "/repo".into(),
            files: files.iter().map(|(change, _)| change.clone()).collect(),
            additions: 0,
            deletions: 0,
            truncated: false,
            settled_at: Utc::now(),
            content: files.into_iter().map(|(_, content)| content).collect(),
        }
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("holt-turn-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn outcome(reply: &TurnRestoreReply, path: &str) -> TurnRestoreOutcome {
        reply
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .outcome
            .clone()
    }

    #[test]
    fn modified_added_and_deleted_files_restore_and_repeat_is_idempotent() {
        let dir = tempdir();
        std::fs::write(dir.join("m.txt"), "new\n").unwrap();
        std::fs::write(dir.join("a.txt"), "created\n").unwrap();
        let rec = record(
            "m-1",
            vec![
                (
                    change("m.txt", TurnFileChangeStatus::Modified, None),
                    content("m.txt", Some("old\n"), Some("new\n")),
                ),
                (
                    change("a.txt", TurnFileChangeStatus::Added, None),
                    content("a.txt", None, Some("created\n")),
                ),
                (
                    change("gone/d.txt", TurnFileChangeStatus::Deleted, None),
                    content("gone/d.txt", Some("kept\n"), None),
                ),
            ],
        );

        let dry = restore(&dir, &rec, &[], &[], true).unwrap();
        assert_eq!(outcome(&dry, "m.txt"), TurnRestoreOutcome::Restored);
        assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "new\n");

        let reply = restore(&dir, &rec, &[], &[], false).unwrap();
        for path in ["m.txt", "a.txt", "gone/d.txt"] {
            assert_eq!(
                outcome(&reply, path),
                TurnRestoreOutcome::Restored,
                "{path}"
            );
        }
        assert_eq!(std::fs::read_to_string(dir.join("m.txt")).unwrap(), "old\n");
        assert!(!dir.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("gone/d.txt")).unwrap(),
            "kept\n"
        );

        let again = restore(&dir, &rec, &[], &[], false).unwrap();
        for file in &again.files {
            assert_eq!(file.outcome, TurnRestoreOutcome::AlreadyRestored);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_edited_after_settlement_is_refused_and_left_alone() {
        let dir = tempdir();
        std::fs::write(dir.join("m.txt"), "hand edited\n").unwrap();
        std::fs::write(dir.join("ok.txt"), "new\n").unwrap();
        let rec = record(
            "m-1",
            vec![
                (
                    change("m.txt", TurnFileChangeStatus::Modified, None),
                    content("m.txt", Some("old\n"), Some("new\n")),
                ),
                (
                    change("ok.txt", TurnFileChangeStatus::Modified, None),
                    content("ok.txt", Some("old\n"), Some("new\n")),
                ),
            ],
        );
        let reply = restore(&dir, &rec, &[], &[], false).unwrap();
        assert_eq!(
            outcome(&reply, "m.txt"),
            refused(TurnRestoreRefusal::Conflict)
        );
        assert_eq!(outcome(&reply, "ok.txt"), TurnRestoreOutcome::Restored);
        assert_eq!(
            std::fs::read_to_string(dir.join("m.txt")).unwrap(),
            "hand edited\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rename_moves_the_old_content_back() {
        let dir = tempdir();
        std::fs::write(dir.join("new.txt"), "body\n").unwrap();
        let rec = record(
            "m-1",
            vec![(
                change("new.txt", TurnFileChangeStatus::Renamed, Some("old.txt")),
                content("new.txt", Some("body\n"), Some("body\n")),
            )],
        );
        // Same content on both sides is a pure rename; the hash gate still
        // applies to the destination.
        let reply = restore(&dir, &rec, &[], &[], false).unwrap();
        assert_eq!(outcome(&reply, "new.txt"), TurnRestoreOutcome::Restored);
        assert!(!dir.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("old.txt")).unwrap(),
            "body\n"
        );
        let again = restore(&dir, &rec, &[], &[], false).unwrap();
        assert_eq!(
            outcome(&again, "new.txt"),
            TurnRestoreOutcome::AlreadyRestored
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_binary_lossy_and_unsafe_records_are_refused() {
        let dir = tempdir();
        std::fs::write(dir.join("t.txt"), "new\n").unwrap();
        let mut truncated = content("t.txt", Some("old\n"), Some("new\n"));
        truncated.truncated = true;
        let mut binary = content("b.bin", None, Some("x"));
        binary.binary = true;
        let mut lossy = content("l.txt", Some("old\n"), Some("new\n"));
        lossy.old_content_hash = Some("not-the-hash-of-the-lossy-text".into());
        std::fs::write(dir.join("l.txt"), "new\n").unwrap();
        let rec = record(
            "m-1",
            vec![
                (
                    change("t.txt", TurnFileChangeStatus::Modified, None),
                    truncated,
                ),
                (
                    change("b.bin", TurnFileChangeStatus::Modified, None),
                    binary,
                ),
                (change("l.txt", TurnFileChangeStatus::Modified, None), lossy),
                (
                    change("../escape.txt", TurnFileChangeStatus::Added, None),
                    content("../escape.txt", None, Some("x")),
                ),
                (
                    change(".git/config", TurnFileChangeStatus::Modified, None),
                    content(".git/config", Some("a"), Some("b")),
                ),
            ],
        );
        let reply = restore(&dir, &rec, &[], &[], false).unwrap();
        assert_eq!(
            outcome(&reply, "t.txt"),
            refused(TurnRestoreRefusal::Truncated)
        );
        assert_eq!(
            outcome(&reply, "b.bin"),
            refused(TurnRestoreRefusal::Binary)
        );
        assert_eq!(
            outcome(&reply, "l.txt"),
            refused(TurnRestoreRefusal::LossyText)
        );
        assert_eq!(
            outcome(&reply, "../escape.txt"),
            refused(TurnRestoreRefusal::UnsafePath)
        );
        assert_eq!(
            outcome(&reply, ".git/config"),
            refused(TurnRestoreRefusal::UnsafePath)
        );
        assert_eq!(std::fs::read_to_string(dir.join("t.txt")).unwrap(), "new\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_later_turn_touching_the_file_is_reported_and_names_the_refusal() {
        let dir = tempdir();
        std::fs::write(dir.join("m.txt"), "later\n").unwrap();
        let first = record(
            "m-1",
            vec![(
                change("m.txt", TurnFileChangeStatus::Modified, None),
                content("m.txt", Some("old\n"), Some("new\n")),
            )],
        );
        let mut second = record(
            "m-2",
            vec![(
                change("m.txt", TurnFileChangeStatus::Modified, None),
                content("m.txt", Some("new\n"), Some("later\n")),
            )],
        );
        second.settled_at = first.settled_at + Duration::seconds(5);

        let reply = restore(&dir, &first, &[first.clone(), second], &[], false).unwrap();
        assert_eq!(
            outcome(&reply, "m.txt"),
            refused(TurnRestoreRefusal::LaterTurn)
        );
        assert_eq!(reply.overlaps.len(), 1);
        assert_eq!(reply.overlaps[0].message_id, "m-2");
        assert_eq!(reply.overlaps[0].paths, vec!["m.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("m.txt")).unwrap(),
            "later\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_outside_the_record_is_a_request_error() {
        let dir = tempdir();
        let rec = record("m-1", Vec::new());
        assert!(restore(&dir, &rec, &[], &["nope.txt".into()], false).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
