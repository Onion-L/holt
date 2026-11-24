//! Rewinding a chat to just before a user message
//! (ADR-rewind-to-a-message-is-all-or-nothing): the plan, the staged
//! confirm, and the durable apply. Every path the undone Turns changed
//! goes back to its bytes before the first of them, by plain file I/O
//! over the checkpoint blobs. All or nothing — a path whose disk no
//! longer holds what the last undone step left, or whose content was
//! never stored, refuses the whole rewind before anything is written
//! (the never-merge stance of ADR-turn-restore-from-change-records).
//!
//! A confirmed rewind is first written down whole (`rewind.json` in the
//! chat's checkpoint directory, synced), then carried out: files, step
//! log, History, transcript, change sets. Every part is idempotent, so a
//! crash anywhere in between is finished by the chat's next load — as is
//! a record the retired step-cut build wrote, under that build's own cut
//! derivation.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use holt_core::agent::types::AgentMessage;
use holt_doc::{MessagePart, MessageRole, SessionMessageEntry};
use holt_proto::{RewindAction, RewindConflict, RewindConflictReason, RewindFile, RewindReply};
use serde::{Deserialize, Serialize};

use crate::checkpoints::Step;
use crate::git::CheckpointSide;

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

/// Where a rewind cuts the History: everything from the target user
/// message on — its Turn and every later one — goes. A message that never
/// ran (and nothing after it) leaves the History as it is.
pub(crate) struct RewindCut {
    /// Messages kept: through just before the first user message of the
    /// cut Turns.
    pub history_len: usize,
    /// The target user message's timestamp — the clock bound the undone
    /// steps are read against.
    pub boundary: i64,
    /// Every call the kept History holds.
    pub retained: HashSet<String>,
}

/// The History cut for a user message stamped `created_at`: user
/// timestamps rise strictly with admission, so the first user message at
/// or after the stamp is the rewind target's own (a message that never
/// ran leaves the next Turn's, which is later still).
pub(crate) fn rewind_cut(history: &[AgentMessage], created_at: i64) -> RewindCut {
    let history_len = history
        .iter()
        .position(
            |message| matches!(message, AgentMessage::User(user) if user.timestamp >= created_at),
        )
        .unwrap_or(history.len());
    RewindCut {
        history_len,
        boundary: created_at,
        retained: history[..history_len].iter().flat_map(tool_calls).collect(),
    }
}

/// The transcript a rewind keeps — everything before the target user
/// entry — and the message ids of the Turns it cuts: the target's own and
/// every user entry after it.
pub(crate) fn transcript_cut(
    transcript: &[SessionMessageEntry],
    message_id: &str,
) -> Result<(Vec<SessionMessageEntry>, Vec<String>), String> {
    let index = transcript
        .iter()
        .position(|entry| entry.id == message_id)
        .ok_or("that message is no longer in the conversation (compacted or rewound)")?;
    if transcript[index].role != MessageRole::User {
        return Err("only a user message can be rewound to".into());
    }
    let kept = transcript[..index].to_vec();
    let cut_turns = transcript[index..]
        .iter()
        .filter(|entry| entry.role == MessageRole::User)
        .map(|entry| entry.id.clone())
        .collect();
    Ok((kept, cut_turns))
}

/// Tool calls owned by the Turns the target cuts. Unlike History, the
/// transcript retains these chips across compaction, so background work is
/// still attributed to the Turn that launched it.
pub(crate) fn cut_tool_calls(
    transcript: &[SessionMessageEntry],
    message_id: &str,
) -> Result<HashSet<String>, String> {
    let index = transcript
        .iter()
        .position(|entry| entry.id == message_id)
        .ok_or("that message is no longer in the conversation (compacted or rewound)")?;
    Ok(transcript[index..]
        .iter()
        .flat_map(|entry| &entry.parts)
        .filter_map(|part| match part {
            MessagePart::Tool { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect())
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
            plan_id: String::new(),
            // The caller sizes the cut (it derives the Turns) and fills the
            // confirm's summary fields; plan only walks the files.
            turns: 0,
            steps: 0,
            will_stop_turn: false,
            pending_count: 0,
            workflows_to_stop: Vec::new(),
            running_processes: 0,
            files,
            conflicts,
            applied: false,
        },
        writes,
    )
}

/// An in-memory staged plan. Confirm carries this opaque identity so a
/// replaced plan can never authorize a different message's cut.
#[derive(Clone, Debug)]
pub(crate) struct StagedRewind {
    pub plan_id: String,
    pub pending: Pending,
}

/// A confirmed rewind, written down before any of it happens. It carries
/// everything the apply needs — the confirm checked the world still held
/// this plan, so a replay after a crash lands the same state without
/// re-deriving anything from a conversation the rewind may have already
/// cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pending {
    pub message_id: String,
    /// The target user message's timestamp — where the History cuts.
    pub created_at: i64,
    /// The cut Turns' message ids: their change sets die with them.
    pub cut_turns: Vec<String>,
    /// `None` when no file changes.
    pub workdir: Option<PathBuf>,
    pub writes: Vec<Write>,
    /// The undone steps' call ids, dropped from the step log.
    pub undone: Vec<String>,
}

/// A rewind written down by the retired step-cut build: the same
/// write-down contract, keyed by a tool call instead of a user message.
/// A chat that crashed between that build's `begin` and its finish loads
/// with this record on disk, so finishing it needs the step-cut
/// derivation the message cut replaced — kept here, for this record only.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyPending {
    tool_call_id: String,
    /// `None` when no file changes.
    workdir: Option<PathBuf>,
    writes: Vec<Write>,
    /// The undone steps' call ids, dropped from the step log.
    undone: Vec<String>,
}

/// Where the retired step cut cut the History: through the results of the
/// assistant message that made the call — parallel siblings stay, so no
/// call is left without its result. Returns the kept length and the
/// call's sibling group (the transcript cut keys on it).
fn legacy_rewind_cut(
    history: &[AgentMessage],
    call_id: &str,
) -> Result<(usize, HashSet<String>), String> {
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
    Ok((end, group))
}

/// The transcript the step cut kept — through the group's last tool chip —
/// and the message ids of the Turns it cut into or dropped (a Turn cut
/// into mid-flight loses its change set with the files it already made).
fn legacy_transcript_cut(
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

fn pending_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    Some(crate::checkpoints::chat_dir(data_dir, chat_id)?.join("rewind.json"))
}

pub(crate) fn pending_exists(data_dir: &Path, chat_id: &str) -> bool {
    pending_path(data_dir, chat_id).is_some_and(|path| path.exists())
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

fn clear_record(data_dir: &Path, chat_id: &str) -> Result<(), String> {
    let Some(path) = pending_path(data_dir, chat_id) else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("clear the rewind record: {error}")),
    }
    crate::store::sync_dir(path.parent().expect("chat dir"))
        .map_err(|error| format!("sync the rewind record removal: {error}"))
}

/// The file writes and the step-log drop every written-down rewind
/// shares, whatever build wrote it down.
fn apply_files_and_log(
    data_dir: &Path,
    chat_id: &str,
    workdir: Option<&Path>,
    writes: &[Write],
    undone: &[String],
) -> Result<(), String> {
    if let Some(workdir) = workdir {
        let store = crate::checkpoints::store_dir(data_dir, chat_id).ok_or("unsafe chat id")?;
        for write in writes {
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
    let undone: HashSet<&str> = undone.iter().map(String::as_str).collect();
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
    Ok(())
}

/// Carry out a written-down rewind against the chat's conversation, on disk
/// and in `history`/`transcript`; returns the Turns whose change sets it
/// retracted. Idempotent: run again after a crash, it lands the same state
/// — every cut reads the pending's own record, never a conversation the
/// rewind may already have cut.
pub(crate) fn finish(
    data_dir: &Path,
    chat_id: &str,
    pending: &Pending,
    history: &mut Vec<AgentMessage>,
    transcript: &mut Vec<SessionMessageEntry>,
) -> Result<Vec<String>, String> {
    apply_files_and_log(
        data_dir,
        chat_id,
        pending.workdir.as_deref(),
        &pending.writes,
        &pending.undone,
    )?;
    let cut = rewind_cut(history, pending.created_at);
    if history.len() > cut.history_len {
        let mut kept = history.clone();
        kept.truncate(cut.history_len);
        crate::history::rewrite(data_dir, chat_id, &kept)
            .map_err(|error| format!("rewrite History during rewind: {error}"))?;
        *history = kept;
    }
    // The target entry is gone only on a replay that already cut it.
    if let Some(index) = transcript
        .iter()
        .position(|entry| entry.id == pending.message_id)
    {
        let entries = transcript[..index].to_vec();
        if entries != *transcript {
            crate::store::rewrite_transcript(data_dir, chat_id, &entries)
                .map_err(|error| error.to_string())?;
            *transcript = entries;
        }
    }
    // The cut Turns' change sets describe a working tree that no longer
    // exists — retracted like an edit-last prune's.
    for message_id in &pending.cut_turns {
        crate::turn_change_store::delete(data_dir, chat_id, message_id);
    }
    clear_record(data_dir, chat_id)?;
    Ok(pending.cut_turns.clone())
}

/// Carry out a written-down rewind the retired step-cut build recorded.
/// The writes and the step log follow the record as the message cut's do;
/// the conversation cuts re-derive from the call the record names. A call
/// the History no longer holds is a replay that already cut it — nothing
/// left to derive, only the record to clear (the old build errored there
/// and left the record stuck; the cut Turn's change set on such a replay
/// is unknowable and stays, stale but inert).
fn finish_legacy(
    data_dir: &Path,
    chat_id: &str,
    pending: &LegacyPending,
    history: &mut Vec<AgentMessage>,
    transcript: &mut Vec<SessionMessageEntry>,
) -> Result<Vec<String>, String> {
    apply_files_and_log(
        data_dir,
        chat_id,
        pending.workdir.as_deref(),
        &pending.writes,
        &pending.undone,
    )?;
    let mut cut_turns = Vec::new();
    if let Ok((history_len, group)) = legacy_rewind_cut(history, &pending.tool_call_id) {
        if history.len() > history_len {
            let mut kept = history.clone();
            kept.truncate(history_len);
            crate::history::rewrite(data_dir, chat_id, &kept)
                .map_err(|error| format!("rewrite History during rewind: {error}"))?;
            *history = kept;
        }
        if let Ok((entries, turns)) = legacy_transcript_cut(transcript, &group) {
            if entries != *transcript {
                crate::store::rewrite_transcript(data_dir, chat_id, &entries)
                    .map_err(|error| error.to_string())?;
                *transcript = entries;
            }
            cut_turns = turns;
        }
    }
    for message_id in &cut_turns {
        crate::turn_change_store::delete(data_dir, chat_id, message_id);
    }
    clear_record(data_dir, chat_id)?;
    Ok(cut_turns)
}

/// Finish a rewind a crash cut off; runs as the chat loads, before anything
/// reads its conversation. A message-cut record also owns the queue sweep:
/// the crash may have happened after the record landed but before the
/// original confirm cleared the queue. Fail closed on any error so later
/// work cannot be cut by a subsequent recovery attempt.
pub(crate) fn finish_pending(
    data_dir: &Path,
    chat_id: &str,
    history: &mut Vec<AgentMessage>,
    transcript: &mut Vec<SessionMessageEntry>,
    queue: &mut crate::queue::Queue,
) -> Result<(), String> {
    let Some(path) = pending_path(data_dir, chat_id) else {
        return Ok(());
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read the rewind record: {error}")),
    };
    match serde_json::from_slice::<Pending>(&bytes) {
        Ok(pending) => {
            queue.recover_rewind().map_err(|error| error.to_string())?;
            finish(data_dir, chat_id, &pending, history, transcript)?;
            queue.finish_rewind_apply();
            Ok(())
        }
        Err(_) => match serde_json::from_slice::<LegacyPending>(&bytes) {
            Ok(legacy) => {
                finish_legacy(data_dir, chat_id, &legacy, history, transcript).map(|_| ())
            }
            Err(error) => Err(format!("unreadable rewind record: {error}")),
        },
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
