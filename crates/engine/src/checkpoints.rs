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

fn chat_dir(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
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

/// Drop a chat's whole checkpoint directory.
pub(crate) fn delete_chat(data_dir: &Path, chat_id: &str) {
    if let Some(dir) = chat_dir(data_dir, chat_id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
