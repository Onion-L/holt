//! Rewinding a chat to a tool step
//! (ADR-rewind-to-a-step-is-all-or-nothing): the plan, and the durable
//! apply. Every path the undone steps changed goes back to its bytes before
//! the first of them, by plain file I/O over the checkpoint blobs. All or
//! nothing — a path whose disk no longer holds what the last undone step
//! left, or whose content was never stored, refuses the whole rewind before
//! anything is written (the never-merge stance of
//! ADR-turn-restore-from-change-records).
//!
//! A confirmed rewind is first written down whole (`rewind.json` in the
//! chat's checkpoint directory, synced), then carried out: files, step log,
//! History, transcript, change sets. Every part is idempotent, so a crash
//! anywhere in between is finished by the chat's next load.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use holt_core::agent::types::AgentMessage;
use holt_doc::{MessagePart, MessageRole, SessionMessageEntry};
use holt_proto::{RewindAction, RewindConflict, RewindConflictReason, RewindFile, RewindReply};
use serde::{Deserialize, Serialize};

use crate::checkpoints::Step;
use crate::git::CheckpointSide;

/// Where a rewind cuts the History.
pub(crate) struct RewindCut {
    /// Messages kept: through the last result of the step's assistant
    /// message.
    pub history_len: usize,
    /// The calls of that assistant message — parallel siblings stay with it.
    pub group: HashSet<String>,
    /// Every call the kept History holds.
    pub retained: HashSet<String>,
    /// The kept tail's newest timestamp: a step recorded later, by a call
    /// the kept History does not hold, is undone. Subagent steps carry the
    /// child's call ids, so only the clock places them.
    pub boundary: i64,
}

fn tool_calls(message: &AgentMessage) -> Vec<String> {
    use holt_core::ai::types::AssistantContent;
    match message {
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn rewind_cut(history: &[AgentMessage], call_id: &str) -> Result<RewindCut, String> {
    let start = history
        .iter()
        .position(|message| tool_calls(message).iter().any(|id| id == call_id))
        .ok_or("that step is no longer in the conversation (compacted or rewound)")?;
    let group: HashSet<String> = tool_calls(&history[start]).into_iter().collect();
    let mut end = start + 1;
    while let Some(AgentMessage::ToolResult(result)) = history.get(end)
        && group.contains(&result.tool_call_id)
    {
        end += 1;
    }
    let boundary = history[start..end]
        .iter()
        .filter_map(AgentMessage::as_llm_message)
        .map(|message| message.timestamp())
        .max()
        .unwrap_or_default();
    Ok(RewindCut {
        history_len: end,
        retained: history[..end].iter().flat_map(tool_calls).collect(),
        group,
        boundary,
    })
}

/// The transcript a rewind keeps — through the step group's last tool chip —
/// and the message ids of the Turns it cuts into or drops.
pub(crate) fn transcript_cut(
    transcript: &[SessionMessageEntry],
    group: &HashSet<String>,
) -> Result<(Vec<SessionMessageEntry>, Vec<String>), String> {
    let is_step =
        |part: &MessagePart| matches!(part, MessagePart::Tool { id, .. } if group.contains(id));
    let (index, last_part) = transcript
        .iter()
        .enumerate()
        .find_map(|(index, entry)| {
            entry
                .parts
                .iter()
                .rposition(is_step)
                .map(|part| (index, part))
        })
        .ok_or("that step is not in the transcript")?;
    let mut kept = transcript[..=index].to_vec();
    kept[index].parts.truncate(last_part + 1);
    let cut_turns = transcript[..index]
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::User)
        .into_iter()
        .chain(
            transcript[index + 1..]
                .iter()
                .filter(|entry| entry.role == MessageRole::User),
        )
        .map(|entry| entry.id.clone())
        .collect();
    Ok((kept, cut_turns))
}

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

/// One file write of a confirmed rewind: the repo-relative path and the
/// side it goes back to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Write {
    path: String,
    target: CheckpointSide,
}

/// What rewinding `undone` (log order) in `workdir` would do. The reply's
/// `applied` is false; the writes are what a confirmed rewind carries out.
pub(crate) fn plan(workdir: &Path, store: &Path, undone: &[Step]) -> (RewindReply, Vec<Write>) {
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
        writes.push(Write {
            path: change.path,
            target: change.target,
        });
    }
    (
        RewindReply {
            files,
            conflicts,
            applied: false,
        },
        writes,
    )
}

/// A confirmed rewind, written down before any of it happens.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pending {
    pub tool_call_id: String,
    /// `None` when no file changes.
    pub workdir: Option<PathBuf>,
    pub writes: Vec<Write>,
    /// The undone steps' call ids, dropped from the step log.
    pub undone: Vec<String>,
}

fn pending_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    Some(crate::checkpoints::chat_dir(data_dir, chat_id)?.join("rewind.json"))
}

/// Write the rewind down, synced, before carrying it out.
pub(crate) fn begin(data_dir: &Path, chat_id: &str, pending: &Pending) -> std::io::Result<()> {
    let path =
        pending_path(data_dir, chat_id).ok_or_else(|| std::io::Error::other("unsafe chat id"))?;
    let dir = path.parent().expect("chat dir");
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".rewind-{}.json", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec(pending).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&temp, &path)?;
    crate::store::sync_dir(dir)
}

/// Carry out a written-down rewind against the chat's conversation, on disk
/// and in `history`/`transcript`; returns the Turns whose change sets it
/// retracted. Idempotent: run again after a crash, it lands the same state.
pub(crate) fn finish(
    data_dir: &Path,
    chat_id: &str,
    pending: &Pending,
    history: &mut Vec<AgentMessage>,
    transcript: &mut Vec<SessionMessageEntry>,
) -> Result<Vec<String>, String> {
    if let Some(workdir) = &pending.workdir {
        let store = crate::checkpoints::store_dir(data_dir, chat_id).ok_or("unsafe chat id")?;
        for write in &pending.writes {
            let path = crate::turn_restore::safe_target(workdir, &write.path)
                .ok_or_else(|| format!("rewind {}: path left the work tree", write.path))?;
            let bytes = side_bytes(&store, &write.target)
                .map_err(|()| format!("rewind {}: content missing", write.path))?;
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
            written.map_err(|error| format!("rewind {}: {error}", write.path))?;
        }
    }
    let undone: HashSet<&str> = pending.undone.iter().map(String::as_str).collect();
    let steps = crate::checkpoints::load(data_dir, chat_id);
    if steps
        .iter()
        .any(|step| undone.contains(step.tool_call_id.as_str()))
    {
        let kept: Vec<Step> = steps
            .into_iter()
            .filter(|step| !undone.contains(step.tool_call_id.as_str()))
            .collect();
        crate::checkpoints::mark_for_repack(data_dir, chat_id);
        crate::checkpoints::rewrite(data_dir, chat_id, &kept)
            .map_err(|error| format!("rewrite the step log: {error}"))?;
    }
    let cut = rewind_cut(history, &pending.tool_call_id)?;
    let (entries, cut_turns) = transcript_cut(transcript, &cut.group)?;
    if history.len() > cut.history_len {
        history.truncate(cut.history_len);
        crate::history::rewrite(data_dir, chat_id, history).map_err(|error| error.to_string())?;
    }
    if entries != *transcript {
        crate::store::rewrite_transcript(data_dir, chat_id, &entries)
            .map_err(|error| error.to_string())?;
        *transcript = entries;
    }
    // The cut Turns' change sets describe a working tree that no longer
    // exists — retracted like an edit-last prune's.
    for message_id in &cut_turns {
        crate::turn_change_store::delete(data_dir, chat_id, message_id);
    }
    if let Some(path) = pending_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
    Ok(cut_turns)
}

/// Finish a rewind a crash cut off; runs as the chat loads, before anything
/// reads its conversation. A rewind that cannot finish stays written down
/// for the next load.
pub(crate) fn finish_pending(
    data_dir: &Path,
    chat_id: &str,
    history: &mut Vec<AgentMessage>,
    transcript: &mut Vec<SessionMessageEntry>,
) {
    let Some(pending) = pending_path(data_dir, chat_id)
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Pending>(&bytes).ok())
    else {
        return;
    };
    if let Err(error) = finish(data_dir, chat_id, &pending, history, transcript) {
        tracing::warn!(target: "holt::checkpoints", %error, "could not finish an interrupted rewind");
    }
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
