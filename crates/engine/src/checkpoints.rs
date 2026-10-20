//! The per-chat step log (ADR-step-checkpoints-in-a-chat-owned-object-store):
//! one line per tool call that changed files in a Git working tree, under
//! `{data_dir}/checkpoints/{chatId}/steps.jsonl`, naming the before/after
//! blobs `objects/` holds (see [`crate::git::CheckpointFile`]).
//!
//! While a call runs, its start waits in `running/{toolCallId}.json`; the
//! call removes it however it ends, so one still there at load was cut off
//! by a crash. Load measures it against the working tree, records an
//! interrupted step, and the History repair tells the model what changed.
//!
//! Writes are best-effort, like the change attribution they ride: a failed
//! write costs that step's checkpoint, never the tool call. Reads skip a
//! line that does not parse — the crash-truncated tail. Deleting a chat
//! deletes its whole directory.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::git::{CallStart, CheckpointFile, CheckpointSide};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Step {
    /// The Turn's identity: the queued user message it started from.
    pub message_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub timestamp: i64,
    /// A crash cut the call off; `files` is what changed between its start
    /// and the next load, by anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    pub files: Vec<CheckpointFile>,
}

/// A running call's start.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Running {
    message_id: String,
    tool_call_id: String,
    tool_name: String,
    cwd: String,
    start: CallStart,
}

pub(crate) fn chat_dir(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    crate::store::id_is_path_safe(chat_id).then(|| data_dir.join("checkpoints").join(chat_id))
}

fn append_step(dir: &Path, step: &Step) {
    let written = serde_json::to_vec(step)
        .map_err(std::io::Error::other)
        .and_then(|mut line| {
            line.push(b'\n');
            std::fs::create_dir_all(dir)?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("steps.jsonl"))?
                .write_all(&line)
        });
    if let Err(error) = written {
        tracing::warn!(target: "holt::checkpoints", %error, "could not record a step checkpoint");
    }
}

/// One Turn's handle on its chat's step log, shared by every tool the Turn
/// (and its subagents) runs.
#[derive(Clone)]
pub(crate) struct StepLog {
    dir: PathBuf,
    message_id: String,
    append: Arc<Mutex<()>>,
}

impl StepLog {
    /// `None` for a chat id that could escape the checkpoints directory.
    pub(crate) fn new(data_dir: &Path, chat_id: &str, message_id: &str) -> Option<Self> {
        Some(Self {
            dir: chat_dir(data_dir, chat_id)?,
            message_id: message_id.to_string(),
            append: Arc::default(),
        })
    }

    pub(crate) fn store(&self) -> PathBuf {
        self.dir.join("objects")
    }

    /// Keep a call's start on disk until the returned guard drops.
    pub(crate) fn begin(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        cwd: &str,
        start: CallStart,
    ) -> Option<RunningCall> {
        if !crate::store::id_is_path_safe(tool_call_id) {
            return None;
        }
        let running = Running {
            message_id: self.message_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            cwd: cwd.to_string(),
            start,
        };
        let dir = self.dir.join("running");
        let path = dir.join(format!("{tool_call_id}.json"));
        let written = serde_json::to_vec(&running)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, bytes)
            });
        match written {
            Ok(()) => Some(RunningCall { path }),
            Err(error) => {
                tracing::warn!(target: "holt::checkpoints", %error, "could not record a call's start");
                None
            }
        }
    }

    /// Record one call's changed files; a call that changed none records
    /// nothing.
    pub(crate) fn append(&self, tool_call_id: &str, tool_name: &str, files: Vec<CheckpointFile>) {
        if files.is_empty() {
            return;
        }
        let step = Step {
            message_id: self.message_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            interrupted: false,
            files,
        };
        let _guard = self
            .append
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        append_step(&self.dir, &step);
    }
}

/// A running call's start on disk; dropping it — the call ended, however —
/// removes it. A hard kill runs no destructor, which is the point.
pub(crate) struct RunningCall {
    path: PathBuf,
}

impl Drop for RunningCall {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Turn every call a crash cut off into an interrupted step. Runs when the
/// chat loads, before anything else of it runs; blocking.
pub(crate) fn settle_interrupted(data_dir: &Path, chat_id: &str) {
    let Some(dir) = chat_dir(data_dir, chat_id) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir.join("running")) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let running = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Running>(&bytes).ok());
        if let Some(running) = running {
            match crate::git::files_since(&running.cwd, &dir.join("objects"), &running.start) {
                Ok(Some(files)) => append_step(
                    &dir,
                    &Step {
                        message_id: running.message_id,
                        tool_call_id: running.tool_call_id,
                        tool_name: running.tool_name,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                        interrupted: true,
                        files,
                    },
                ),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(target: "holt::checkpoints", %error, "could not measure an interrupted call");
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// For each of `calls` with a recorded step, one sentence naming the files
/// it changed — the History repair's addition to a cut-off call's result.
pub(crate) fn change_notes(
    data_dir: &Path,
    chat_id: &str,
    calls: &HashSet<String>,
) -> HashMap<String, String> {
    let Some(bytes) =
        chat_dir(data_dir, chat_id).and_then(|dir| std::fs::read(dir.join("steps.jsonl")).ok())
    else {
        return HashMap::new();
    };
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Step>(line).ok())
        .filter(|step| calls.contains(&step.tool_call_id))
        .map(|step| {
            let note = change_note(&step);
            (step.tool_call_id, note)
        })
        .collect()
}

/// Paths listed before the rest is counted.
const NOTED_PATHS: usize = 20;

fn change_note(step: &Step) -> String {
    if step.files.is_empty() {
        return "Since it started, no file in the working tree changed.".into();
    }
    let mut listed: Vec<String> = step
        .files
        .iter()
        .take(NOTED_PATHS)
        .map(|file| {
            let how = match (&file.before, &file.after) {
                (CheckpointSide::Absent, _) => "created",
                (_, CheckpointSide::Absent) => "deleted",
                _ => "modified",
            };
            format!("{} ({how})", file.path)
        })
        .collect();
    if step.files.len() > NOTED_PATHS {
        listed.push(format!("{} more", step.files.len() - NOTED_PATHS));
    }
    let listed = listed.join(", ");
    if step.interrupted {
        format!("Since it started, these files in the working tree changed: {listed}.")
    } else {
        format!(
            "It ran to completion before the restart, but its output was lost; it changed these \
             files in the working tree: {listed}."
        )
    }
}

/// A chat's checkpoint store.
pub(crate) fn store_dir(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    Some(chat_dir(data_dir, chat_id)?.join("objects"))
}

/// A chat's recorded steps, in log order.
pub(crate) fn load(data_dir: &Path, chat_id: &str) -> Vec<Step> {
    let Some(bytes) =
        chat_dir(data_dir, chat_id).and_then(|dir| std::fs::read(dir.join("steps.jsonl")).ok())
    else {
        return Vec::new();
    };
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// Replace the step log with `steps` (a rewind dropping what it undid):
/// tmp + rename, so a crash leaves the old log or the new one.
pub(crate) fn rewrite(data_dir: &Path, chat_id: &str, steps: &[Step]) -> std::io::Result<()> {
    let dir = chat_dir(data_dir, chat_id).ok_or_else(|| std::io::Error::other("unsafe chat id"))?;
    std::fs::create_dir_all(&dir)?;
    let mut bytes = Vec::new();
    for step in steps {
        serde_json::to_writer(&mut bytes, step).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
    }
    let temp = dir.join(format!(".steps-{}.jsonl", uuid::Uuid::new_v4()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, dir.join("steps.jsonl")).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Steps a chat keeps: past `max`, the oldest go down to `keep`.
const KEEP_STEPS: usize = 1000;
const MAX_STEPS: usize = 1200;
/// The store's byte budget: over it, the oldest fifth of the steps go until
/// it fits.
const MAX_STORE_BYTES: u64 = 1 << 30;

struct Limits {
    keep: usize,
    max: usize,
    bytes: u64,
}

/// Keep a chat's checkpoints bounded (ADR-step-checkpoints-in-a-chat-owned-object-store):
/// drop the oldest steps past the step and byte limits, and repack the
/// store to what the remaining steps reference — also after a rewind left
/// blobs nothing references. Runs at Turn admission, under the chat's
/// execution lock, so no call is writing to the store; blocking.
pub(crate) fn maintain(data_dir: &Path, chat_id: &str) {
    maintain_within(
        data_dir,
        chat_id,
        &Limits {
            keep: KEEP_STEPS,
            max: MAX_STEPS,
            bytes: MAX_STORE_BYTES,
        },
    );
}

fn maintain_within(data_dir: &Path, chat_id: &str, limits: &Limits) {
    let Some(dir) = chat_dir(data_dir, chat_id) else {
        return;
    };
    // A rewind still to finish needs every blob it names.
    if !dir.exists() || dir.join("rewind.json").exists() {
        return;
    }
    if let Err(error) = maintain_dir(data_dir, chat_id, &dir, limits) {
        tracing::warn!(target: "holt::checkpoints", %error, "could not trim the checkpoints");
    }
}

fn maintain_dir(data_dir: &Path, chat_id: &str, dir: &Path, limits: &Limits) -> Result<(), String> {
    let store = dir.join("objects");
    let fresh = dir.join("objects.new");
    let stale = dir.join("objects.old");
    // A swap a crash cut off: the new store was complete before the old one
    // moved aside.
    if !store.exists() && fresh.exists() {
        std::fs::rename(&fresh, &store).map_err(|error| error.to_string())?;
    }
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&stale);

    let marker = dir.join("repack");
    let mut steps = load(data_dir, chat_id);
    let mut drop = steps.len().saturating_sub(if steps.len() > limits.max {
        limits.keep
    } else {
        steps.len()
    });
    let mut repack = marker.exists();
    loop {
        if drop > 0 {
            let dropped: Vec<Step> = steps.drain(..drop).collect();
            // Marker and limit first: a crash before the repack leaves both
            // to the next run, never a log that silently lost steps.
            std::fs::write(&marker, b"").map_err(|error| error.to_string())?;
            note_dropped(dir, &dropped).map_err(|error| error.to_string())?;
            rewrite(data_dir, chat_id, &steps).map_err(|error| error.to_string())?;
            repack = true;
        }
        if repack && store.exists() {
            let keep: HashSet<String> = steps
                .iter()
                .flat_map(|step| &step.files)
                .flat_map(|file| [&file.before, &file.after])
                .filter_map(|side| match side {
                    CheckpointSide::Blob { id } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            crate::git::copy_checkpoint_blobs(&store, &fresh, &keep)?;
            std::fs::rename(&store, &stale).map_err(|error| error.to_string())?;
            std::fs::rename(&fresh, &store).map_err(|error| error.to_string())?;
            let _ = std::fs::remove_dir_all(&stale);
        }
        let _ = std::fs::remove_file(&marker);
        repack = false;
        if steps.is_empty() || dir_bytes(&store) <= limits.bytes {
            return Ok(());
        }
        drop = steps.len().div_ceil(5);
    }
}

/// Remember the newest dropped step: a rewind to before it could no longer
/// undo everything after its point.
fn note_dropped(dir: &Path, dropped: &[Step]) -> std::io::Result<()> {
    let Some(newest) = dropped.iter().map(|step| step.timestamp).max() else {
        return Ok(());
    };
    let until = read_dropped(dir).map_or(newest, |until| until.max(newest));
    std::fs::write(
        dir.join("dropped.json"),
        serde_json::to_vec(&serde_json::json!({ "until": until }))
            .map_err(std::io::Error::other)?,
    )
}

fn read_dropped(dir: &Path) -> Option<i64> {
    let bytes = std::fs::read(dir.join("dropped.json")).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("until")?
        .as_i64()
}

/// The newest step the chat's limits dropped, if any.
pub(crate) fn dropped_until(data_dir: &Path, chat_id: &str) -> Option<i64> {
    read_dropped(&chat_dir(data_dir, chat_id)?)
}

/// Ask the next maintenance to repack: a rewind dropped steps.
pub(crate) fn mark_for_repack(data_dir: &Path, chat_id: &str) {
    if let Some(dir) = chat_dir(data_dir, chat_id) {
        let _ = std::fs::write(dir.join("repack"), b"");
    }
}

fn dir_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_bytes(&entry.path()),
            _ => entry.metadata().map_or(0, |meta| meta.len()),
        })
        .sum()
}

/// Drop a chat's whole checkpoint directory.
pub(crate) fn delete_chat(data_dir: &Path, chat_id: &str) {
    if let Some(dir) = chat_dir(data_dir, chat_id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chat with `count` steps, each changing one file to unique bytes of
    /// `size`, timestamped 1, 2, ….
    fn chat_with_steps(data_dir: &Path, count: usize, size: usize) -> PathBuf {
        let dir = chat_dir(data_dir, "chat-1").unwrap();
        let store = dir.join("objects");
        let steps: Vec<Step> = (0..count)
            .map(|n| {
                let mut bytes = format!("step {n}\n").into_bytes();
                bytes.resize(size.max(bytes.len()), b'x');
                Step {
                    message_id: "m".into(),
                    tool_call_id: format!("c{n}"),
                    tool_name: "write".into(),
                    timestamp: n as i64 + 1,
                    interrupted: false,
                    files: vec![CheckpointFile {
                        path: format!("f{n}"),
                        before: CheckpointSide::Absent,
                        after: CheckpointSide::Blob {
                            id: crate::git::put_checkpoint_blob(&store, &bytes),
                        },
                    }],
                }
            })
            .collect();
        rewrite(data_dir, "chat-1", &steps).unwrap();
        dir
    }

    fn blob_of(step: &Step) -> String {
        match &step.files[0].after {
            CheckpointSide::Blob { id } => id.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn past_the_step_limit_the_oldest_go_and_their_blobs_with_them() {
        let data = tempfile::tempdir().unwrap();
        let dir = chat_with_steps(data.path(), 12, 0);
        let before = load(data.path(), "chat-1");
        let limits = Limits {
            keep: 8,
            max: 10,
            bytes: u64::MAX,
        };
        maintain_within(data.path(), "chat-1", &limits);

        let after = load(data.path(), "chat-1");
        assert_eq!(after, before[4..]);
        assert_eq!(dropped_until(data.path(), "chat-1"), Some(4));
        let store = dir.join("objects");
        assert!(crate::git::checkpoint_blob(&store, &blob_of(&before[3])).is_none());
        assert!(crate::git::checkpoint_blob(&store, &blob_of(&before[4])).is_some());
        assert!(!dir.join("repack").exists());

        // Under the limit again: nothing more goes.
        maintain_within(data.path(), "chat-1", &limits);
        assert_eq!(load(data.path(), "chat-1").len(), 8);
    }

    #[test]
    fn over_the_byte_budget_the_oldest_fifths_go_until_it_fits() {
        let data = tempfile::tempdir().unwrap();
        // Incompressible enough: each blob is distinct, 64 KiB.
        let dir = chat_with_steps(data.path(), 10, 64 * 1024);
        let budget = dir_bytes(&dir.join("objects")) / 2;
        maintain_within(
            data.path(),
            "chat-1",
            &Limits {
                keep: 100,
                max: 100,
                bytes: budget,
            },
        );
        let kept = load(data.path(), "chat-1");
        assert!(!kept.is_empty() && kept.len() < 10, "{}", kept.len());
        assert!(dir_bytes(&dir.join("objects")) <= budget);
        assert_eq!(kept.last().unwrap().tool_call_id, "c9", "the newest stay");
    }

    #[test]
    fn a_repack_mark_reclaims_blobs_no_step_references() {
        let data = tempfile::tempdir().unwrap();
        let dir = chat_with_steps(data.path(), 2, 0);
        let orphan = crate::git::put_checkpoint_blob(&dir.join("objects"), b"rewound away");
        mark_for_repack(data.path(), "chat-1");
        maintain(data.path(), "chat-1");
        assert!(crate::git::checkpoint_blob(&dir.join("objects"), &orphan).is_none());
        let steps = load(data.path(), "chat-1");
        assert_eq!(steps.len(), 2);
        assert!(crate::git::checkpoint_blob(&dir.join("objects"), &blob_of(&steps[1])).is_some());
        assert_eq!(dropped_until(data.path(), "chat-1"), None);
    }

    #[test]
    fn a_store_swap_a_crash_cut_off_is_completed() {
        let data = tempfile::tempdir().unwrap();
        let dir = chat_with_steps(data.path(), 1, 0);
        let steps = load(data.path(), "chat-1");
        // Crashed between moving the old store aside and moving the new
        // one in.
        std::fs::rename(dir.join("objects"), dir.join("objects.new")).unwrap();
        maintain(data.path(), "chat-1");
        assert!(crate::git::checkpoint_blob(&dir.join("objects"), &blob_of(&steps[0])).is_some());
        assert!(!dir.join("objects.new").exists());
    }

    #[test]
    fn a_pending_rewind_holds_off_maintenance() {
        let data = tempfile::tempdir().unwrap();
        let dir = chat_with_steps(data.path(), 12, 0);
        std::fs::write(dir.join("rewind.json"), b"{}").unwrap();
        maintain_within(
            data.path(),
            "chat-1",
            &Limits {
                keep: 8,
                max: 10,
                bytes: u64::MAX,
            },
        );
        assert_eq!(load(data.path(), "chat-1").len(), 12);
    }
}
