//! Durable Turn change-set history (ADR-0024, ticket 02): one JSON record
//! per settled Turn under `{data_dir}/turn-changes/{chatId}/{messageId}.json`
//! — the frozen summary plus the immutable per-file before/after content.
//!
//! Records are written once, at Turn settlement — a replayed settle for the
//! same Turn atomically replaces its own file, nothing else ever rewrites
//! one: a historical Turn's Review answers from these bytes, so restarts
//! and later workspace edits cannot move it. Writes are atomic (tmp +
//! rename + fsync, the queue-file pattern); reads are lazy per request —
//! nothing loads at boot. A malformed record degrades to "no record" with
//! a warning: a damaged change-set history must never block the chat.
//! Deleting a chat deletes its whole `turn-changes/{chatId}` directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use holt_proto::{TurnChangeSet, TurnChangeSetPhase, TurnFileChange};
use serde::{Deserialize, Serialize};

use crate::git::TurnFileContent;

/// One settled Turn's frozen change set, exactly as it was at settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnChangeRecord {
    /// The Turn's identity: the queued user message it started from.
    pub message_id: String,
    /// The working directory the change was captured from — provenance only;
    /// reads never touch it again.
    pub cwd: String,
    pub files: Vec<TurnFileChange>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    /// When the Turn settled — the change set's stable `updatedAt`.
    pub settled_at: DateTime<Utc>,
    /// The immutable per-file before/after pairs, sorted by path.
    pub content: Vec<TurnFileContent>,
}

impl TurnChangeRecord {
    /// Build the durable record from one settle-time freeze: summary and
    /// content captured together, stamped with the freeze time.
    pub(crate) fn frozen(
        message_id: &str,
        cwd: &str,
        freeze: &crate::git::TurnChangeFreeze,
    ) -> Self {
        Self {
            message_id: message_id.to_string(),
            cwd: cwd.to_string(),
            files: freeze.capture.files.clone(),
            additions: freeze.capture.additions,
            deletions: freeze.capture.deletions,
            truncated: freeze.capture.truncated,
            settled_at: Utc::now(),
            content: freeze.content.clone(),
        }
    }

    /// The wire change set this record restores: always `final`.
    pub(crate) fn change_set(&self, chat_id: &str) -> TurnChangeSet {
        TurnChangeSet {
            chat_id: chat_id.to_string(),
            message_id: self.message_id.clone(),
            phase: TurnChangeSetPhase::Final,
            files: self.files.clone(),
            additions: self.additions,
            deletions: self.deletions,
            truncated: self.truncated,
            updated_at: self.settled_at,
        }
    }

    /// The immutable before/after pair of one path in this record.
    pub(crate) fn content_for(&self, path: &str) -> Option<&TurnFileContent> {
        self.content.iter().find(|entry| entry.path == path)
    }
}

/// A record's file path, or `None` when either id could escape the
/// `turn-changes` directory (the transcripts' path-safety rule).
pub(crate) fn record_path(data_dir: &Path, chat_id: &str, message_id: &str) -> Option<PathBuf> {
    if !crate::store::id_is_path_safe(chat_id) || !crate::store::id_is_path_safe(message_id) {
        return None;
    }
    Some(
        data_dir
            .join("turn-changes")
            .join(chat_id)
            .join(format!("{message_id}.json")),
    )
}

/// Durably write one settled Turn's record. One record per Turn identity:
/// an idempotent re-write of the same Turn replaces its own file.
pub(crate) fn save(
    data_dir: &Path,
    chat_id: &str,
    record: &TurnChangeRecord,
) -> Result<(), String> {
    let Some(path) = record_path(data_dir, chat_id, &record.message_id) else {
        return Ok(());
    };
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().expect("record directory");
        std::fs::create_dir_all(parent)?;
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(record)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temp);
        result
    };
    write().map_err(|error| error.to_string())
}

/// Load one settled Turn's record. `None` when it was never written, an id
/// is path-hostile, or the file is unreadable or malformed — a damaged
/// record degrades to historylessness, never a chat failure.
pub(crate) fn load(data_dir: &Path, chat_id: &str, message_id: &str) -> Option<TurnChangeRecord> {
    let path = record_path(data_dir, chat_id, message_id)?;
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<TurnChangeRecord>(&bytes) {
        Ok(record) => Some(record),
        Err(error) => {
            tracing::warn!(
                target: "holt::turn_changes",
                path = %path.display(),
                %error,
                "ignoring a malformed Turn change-set record"
            );
            None
        }
    }
}

/// Drop a chat's whole persisted change-set history. Missing directories
/// are fine — chats whose Turns never captured have nothing on disk.
pub(crate) fn delete_chat(data_dir: &Path, chat_id: &str) {
    if crate::store::id_is_path_safe(chat_id) {
        let _ = std::fs::remove_dir_all(data_dir.join("turn-changes").join(chat_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(message_id: &str) -> TurnChangeRecord {
        TurnChangeRecord {
            message_id: message_id.into(),
            cwd: "/repo".into(),
            files: vec![TurnFileChange {
                path: "src/lib.rs".into(),
                old_path: None,
                status: holt_proto::TurnFileChangeStatus::Modified,
                additions: 3,
                deletions: 1,
                binary: false,
            }],
            additions: 3,
            deletions: 1,
            truncated: false,
            settled_at: Utc::now(),
            content: vec![TurnFileContent {
                path: "src/lib.rs".into(),
                old_text: Some("old\n".into()),
                new_text: Some("new\n".into()),
                old_content_hash: Some("old-hash".into()),
                new_content_hash: Some("new-hash".into()),
                binary: false,
                truncated: false,
            }],
        }
    }

    #[test]
    fn records_round_trip_and_unsafe_ids_neither_read_nor_write() {
        let dir = std::env::temp_dir().join(format!("holt-turn-changes-{}", uuid::Uuid::new_v4()));
        let written = record("m-1");
        save(&dir, "chat-1", &written).expect("save");
        let loaded = load(&dir, "chat-1", "m-1").expect("load");
        assert_eq!(loaded, written);

        // Path-hostile ids stay inside the turn-changes tree.
        save(&dir, "../escape", &record("m-2")).expect("save skipped");
        assert!(load(&dir, "../escape", "m-2").is_none());
        save(&dir, "chat-1", &record("../../escape")).expect("save skipped");
        assert!(!dir.join("escape.json").exists());

        // A missing record is a plain None, and a truncated file degrades
        // the same way instead of failing the chat.
        assert!(load(&dir, "chat-1", "never-settled").is_none());
        std::fs::write(
            dir.join("turn-changes/chat-1/malformed.json"),
            "{\"messageId\"",
        )
        .unwrap();
        assert!(load(&dir, "chat-1", "malformed").is_none());

        delete_chat(&dir, "chat-1");
        assert!(!dir.join("turn-changes/chat-1").exists());
        delete_chat(&dir, "chat-1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
