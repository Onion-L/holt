//! The History record (ADR-0010): a chat's model-facing message sequence
//! persisted as its own append-only JSONL file next to the Transcript — a
//! version header line, then one entry per line. The only entry kind today
//! is `message` (an [`AgentMessage`] in its upstream serde shape,
//! unchanged); the file is holt-owned, so later kinds can be added without
//! binding to upstream session codecs. Reads are tolerant: unknown entry
//! kinds are skipped (unknown message roles already route to the upstream
//! `Custom` variant) and a truncated or unparsable trailing line — the
//! crash-mid-append shape — is treated as absent.

use std::io::Write;
use std::path::{Path, PathBuf};

use pi_core::agent::types::AgentMessage;

use crate::store::chat_id_is_path_safe;

/// The History format version carried by the header line.
const HISTORY_VERSION: u32 = 1;

/// Per-chat History file, guarded by the same chat-id path-safety rule as
/// the Transcript (see [`crate::store::transcript_path`]).
pub(crate) fn history_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !chat_id_is_path_safe(chat_id) {
        return None;
    }
    Some(data_dir.join("history").join(format!("{chat_id}.jsonl")))
}

/// One JSONL entry: `{"kind":"message","entry":{…AgentMessage…}}`. The
/// adjacent-tag shape keeps the entry self-describing for the tolerant
/// reader below (a `compaction` kind joins with the compaction slice).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "entry", rename_all = "camelCase")]
pub(crate) enum HistoryEntry {
    Message(AgentMessage),
}

/// Append one entry, creating the file (header first) when the chat has no
/// History yet. Append-only by construction: a Turn's messages land as
/// they complete, never as a whole-file rewrite.
pub(crate) fn append_entry(
    data_dir: &Path,
    chat_id: &str,
    entry: &HistoryEntry,
) -> std::io::Result<()> {
    let Some(path) = history_path(data_dir, chat_id) else {
        return Ok(());
    };
    let dir = path
        .parent()
        .expect("history path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let line =
        serde_json::to_string(entry).map_err(|error| std::io::Error::other(error.to_string()))?;
    let fresh = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if fresh {
        let header = serde_json::json!({ "version": HISTORY_VERSION });
        writeln!(file, "{header}")?;
    }
    writeln!(file, "{line}")
}

/// Append one completed message to the chat's History.
pub(crate) fn append_message(
    data_dir: &Path,
    chat_id: &str,
    message: &AgentMessage,
) -> std::io::Result<()> {
    append_entry(data_dir, chat_id, &HistoryEntry::Message(message.clone()))
}

/// Replay the History linearly into the in-memory message sequence. A
/// missing file is an empty History (a chat that never ran — or a legacy
/// chat, whose notice arrives with its slice); an unreadable header or an
/// unknown version is an error the caller decides how to surface.
pub(crate) fn load(data_dir: &Path, chat_id: &str) -> Result<Vec<AgentMessage>, String> {
    let Some(path) = history_path(data_dir, chat_id) else {
        return Ok(Vec::new());
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // A chat that never ran — or whose id fails the path-safety rule —
        // has no History: one "no History" shape for callers.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("{}: history file has no header line", path.display()))?;
    let version = serde_json::from_str::<serde_json::Value>(header)
        .ok()
        .and_then(|value| value.get("version").and_then(|v| v.as_u64()))
        .ok_or_else(|| format!("{}: unreadable history header", path.display()))?;
    if version != HISTORY_VERSION as u64 {
        return Err(format!(
            "{}: unknown history format version {version} (supported: {HISTORY_VERSION})",
            path.display()
        ));
    }
    let mut messages = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A truncated or unparsable trailing line is the crash-mid-
            // append shape: treat it as absent rather than failing the
            // whole record.
            tracing::warn!(target: "holt::history", "skipping unparsable history line");
            continue;
        };
        match value.get("kind").and_then(|kind| kind.as_str()) {
            Some("message") => match serde_json::from_value::<AgentMessage>(
                value
                    .get("entry")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            ) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    tracing::warn!(target: "holt::history", %error, "skipping undecodable history entry")
                }
            },
            other => {
                // Unknown entry kinds are tolerated: a future holt wrote
                // them, an older model must not choke on them.
                tracing::warn!(target: "holt::history", ?other, "skipping unknown history entry kind");
            }
        }
    }
    Ok(messages)
}

/// Drop a chat's persisted History. Missing files are fine — chats that
/// never ran have nothing on disk.
pub(crate) fn delete_history(data_dir: &Path, chat_id: &str) {
    if let Some(path) = history_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{AssistantMessage, RoleUser, UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(text.into()),
            timestamp: 1,
        })
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![pi_core::ai::types::AssistantContent::Text(
                pi_core::ai::types::TextContent {
                    text: text.into(),
                    ..Default::default()
                },
            )],
            model: "mock".into(),
            ..Default::default()
        }))
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("holt-history-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn round_trips_messages_behind_a_version_header() {
        let dir = temp_dir();
        append_message(&dir, "chat-1", &user("hello")).unwrap();
        append_message(&dir, "chat-1", &assistant("hi there")).unwrap();
        append_message(&dir, "chat-1", &user("again")).unwrap();
        assert_eq!(
            load(&dir, "chat-1").unwrap(),
            vec![user("hello"), assistant("hi there"), user("again")]
        );

        // The first line is the version header; every line after it is one
        // JSON object per entry.
        let text = std::fs::read_to_string(dir.join("history/chat-1.jsonl")).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], serde_json::json!(1));
        let entries: Vec<serde_json::Value> =
            lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|entry| entry["kind"] == "message"));
        // The message rides in its upstream serde shape: a user entry keeps
        // its role field verbatim.
        assert_eq!(entries[0]["entry"]["role"], "user");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_or_unsafe_chat_id_loads_empty_and_writes_nothing() {
        let dir = temp_dir();
        assert!(load(&dir, "never-ran").unwrap().is_empty());
        // Path-hostile ids neither read nor write outside the history dir.
        append_message(&dir, "../escape", &user("x")).unwrap();
        assert!(load(&dir, "../escape").unwrap().is_empty());
        assert!(!dir.join("escape.jsonl").exists());
        match dir.join("history").read_dir() {
            Ok(mut entries) => assert!(entries.next().is_none()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("unreadable history dir: {error}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_entry_kinds_and_truncated_tails_are_tolerated() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(
            dir.join("history/chat-1.jsonl"),
            concat!(
                "{\"version\":1}\n",
                "{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"before\",\"timestamp\":1}}\n",
                "{\"kind\":\"some-future-kind\",\"entry\":{}}\n",
                "{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"after\",\"timestamp\":2}}\n",
                // A trailing line cut mid-object — the crash-mid-append
                // shape — is treated as absent.
                "{\"kind\":\"mess",
            ),
        )
        .unwrap();

        let loaded = load(&dir, "chat-1").unwrap();
        let texts: Vec<&str> = loaded
            .iter()
            .filter_map(|message| match message {
                AgentMessage::User(message) => match &message.content {
                    UserContent::Text(text) => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["before", "after"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_version_is_an_error_not_a_silent_wipe() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(
            dir.join("history/chat-1.jsonl"),
            "{\"version\":99}\n{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        )
        .unwrap();
        assert!(load(&dir, "chat-1").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_only_that_chats_history() {
        let dir = temp_dir();
        append_message(&dir, "chat-1", &user("x")).unwrap();
        append_message(&dir, "chat-2", &user("y")).unwrap();
        delete_history(&dir, "chat-1");
        assert!(load(&dir, "chat-1").unwrap().is_empty());
        assert_eq!(load(&dir, "chat-2").unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
