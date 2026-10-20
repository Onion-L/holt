//! The per-chat step log (ADR-step-checkpoints-in-a-chat-owned-object-store):
//! one line per tool call that changed files in a Git working tree, under
//! `{data_dir}/checkpoints/{chatId}/steps.jsonl`, naming the before/after
//! blobs `objects/` holds (see [`crate::git::CheckpointFile`]).
//!
//! Appends are best-effort, like the change attribution they ride: a failed
//! write costs that step's checkpoint, never the tool call. Reads skip a
//! line that does not parse — the crash-truncated tail. Deleting a chat
//! deletes its whole directory.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::git::CheckpointFile;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Step {
    /// The Turn's identity: the queued user message it started from.
    pub message_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub timestamp: i64,
    pub files: Vec<CheckpointFile>,
}

fn chat_dir(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    crate::store::id_is_path_safe(chat_id).then(|| data_dir.join("checkpoints").join(chat_id))
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
            files,
        };
        let _guard = self
            .append
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let written = serde_json::to_vec(&step)
            .map_err(std::io::Error::other)
            .and_then(|mut line| {
                line.push(b'\n');
                std::fs::create_dir_all(&self.dir)?;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.dir.join("steps.jsonl"))?
                    .write_all(&line)
            });
        if let Err(error) = written {
            tracing::warn!(target: "holt::checkpoints", %error, "could not record a step checkpoint");
        }
    }
}

/// Drop a chat's whole checkpoint directory.
pub(crate) fn delete_chat(data_dir: &Path, chat_id: &str) {
    if let Some(dir) = chat_dir(data_dir, chat_id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
