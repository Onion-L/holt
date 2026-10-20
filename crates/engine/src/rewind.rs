//! A rewind's file half (ADR-step-checkpoints-in-a-chat-owned-object-store):
//! every path the undone steps changed goes back to its bytes before the
//! first of them, by plain file I/O over the checkpoint blobs. All or
//! nothing — a path whose disk no longer holds what the last undone step
//! left, or whose content was never stored, refuses the whole rewind before
//! anything is written (the never-merge stance of
//! ADR-turn-restore-from-change-records).

use std::collections::HashMap;
use std::path::Path;

use holt_proto::{RewindAction, RewindConflict, RewindConflictReason, RewindFile, RewindReply};

use crate::checkpoints::Step;
use crate::git::CheckpointSide;

/// One path across the undone steps: where it goes back to, and what the
/// disk must still hold.
struct NetChange {
    path: String,
    target: CheckpointSide,
    expected: CheckpointSide,
}

/// The undone steps' net change per path, in first-touched order. A path
/// they left as they found it needs nothing.
fn net_changes(undone: &[Step]) -> Vec<NetChange> {
    let mut changes: Vec<NetChange> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for file in undone.iter().flat_map(|step| &step.files) {
        match index.get(&file.path) {
            Some(&at) => changes[at].expected = file.after.clone(),
            None => {
                index.insert(file.path.clone(), changes.len());
                changes.push(NetChange {
                    path: file.path.clone(),
                    target: file.before.clone(),
                    expected: file.after.clone(),
                });
            }
        }
    }
    changes.retain(|change| change.target != change.expected);
    changes
}

/// A side's bytes: `Ok(None)` for an absent file, `Err` when the content
/// was never stored or the blob is gone.
fn side_bytes(store: &Path, side: &CheckpointSide) -> Result<Option<Vec<u8>>, ()> {
    match side {
        CheckpointSide::Absent => Ok(None),
        CheckpointSide::Blob { id } => crate::git::checkpoint_blob(store, id).map(Some).ok_or(()),
        CheckpointSide::Omitted => Err(()),
    }
}

/// Plan the rewind of `undone` (log order) in `workdir`, and with
/// `dry_run` false and no conflict, write it.
pub(crate) fn rewind_files(
    workdir: &Path,
    store: &Path,
    undone: &[Step],
    dry_run: bool,
) -> Result<RewindReply, String> {
    let mut files = Vec::new();
    let mut conflicts = Vec::new();
    let mut writes = Vec::new();
    for change in net_changes(undone) {
        let conflict = |reason| RewindConflict {
            path: change.path.clone(),
            reason,
        };
        let Some(target_path) = crate::turn_restore::safe_target(workdir, &change.path) else {
            conflicts.push(conflict(RewindConflictReason::Unsafe));
            continue;
        };
        let (Ok(target), Ok(expected)) = (
            side_bytes(store, &change.target),
            side_bytes(store, &change.expected),
        ) else {
            conflicts.push(conflict(RewindConflictReason::NotStored));
            continue;
        };
        let current = match std::fs::read(&target_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                conflicts.push(conflict(RewindConflictReason::Changed));
                continue;
            }
        };
        if current != expected {
            conflicts.push(conflict(RewindConflictReason::Changed));
            continue;
        }
        files.push(RewindFile {
            path: change.path.clone(),
            action: if target.is_some() {
                RewindAction::Restore
            } else {
                RewindAction::Delete
            },
        });
        writes.push((target_path, target));
    }
    let applied = !dry_run && conflicts.is_empty();
    if applied {
        for (path, bytes) in writes {
            let written = match bytes {
                Some(bytes) => crate::turn_restore::write_atomic(&path, &bytes),
                None => std::fs::remove_file(&path).or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }),
            };
            written.map_err(|error| format!("rewind {}: {error}", path.display()))?;
        }
    }
    Ok(RewindReply {
        files,
        conflicts,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::CheckpointFile;

    fn blob(id: &str) -> CheckpointSide {
        CheckpointSide::Blob { id: id.into() }
    }

    fn step(files: Vec<(&str, CheckpointSide, CheckpointSide)>) -> Step {
        Step {
            message_id: "m".into(),
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            timestamp: 0,
            interrupted: false,
            files: files
                .into_iter()
                .map(|(path, before, after)| CheckpointFile {
                    path: path.into(),
                    before,
                    after,
                })
                .collect(),
        }
    }

    #[test]
    fn a_path_goes_back_to_its_first_before_and_expects_its_last_after() {
        let undone = [
            step(vec![
                ("a", blob("a0"), blob("a1")),
                ("b", CheckpointSide::Absent, blob("b1")),
            ]),
            step(vec![
                ("a", blob("a1"), blob("a2")),
                // Created then removed again: nothing to put back.
                ("b", blob("b1"), CheckpointSide::Absent),
            ]),
        ];
        let changes = net_changes(&undone);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "a");
        assert_eq!(changes[0].target, blob("a0"));
        assert_eq!(changes[0].expected, blob("a2"));
    }
}
