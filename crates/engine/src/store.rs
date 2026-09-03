//! JSON persistence for spaces/chats and the stable device id, written
//! atomically (tmp + rename) under the data dir.

use std::path::{Path, PathBuf};

use holt_doc::SessionMessageEntry;
use holt_proto::{Chat, Space};

use crate::EngineError;

pub(crate) fn spaces_path(data_dir: &Path) -> PathBuf {
    data_dir.join("spaces.json")
}

pub(crate) fn chats_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chats.json")
}

pub(crate) fn load_chats(data_dir: &Path) -> Result<Vec<Chat>, EngineError> {
    let path = chats_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn persist_chats(data_dir: &Path, chats: &[Chat]) -> Result<(), EngineError> {
    let path = chats_path(data_dir);
    let temp_path = data_dir.join("chats.json.tmp");
    let bytes =
        serde_json::to_vec_pretty(chats).map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

pub(crate) fn load_spaces(data_dir: &Path) -> Result<Vec<Space>, EngineError> {
    let path = spaces_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn persist_spaces(data_dir: &Path, spaces: &[Space]) -> Result<(), EngineError> {
    let path = spaces_path(data_dir);
    let temp_path = data_dir.join("spaces.json.tmp");
    let bytes =
        serde_json::to_vec_pretty(spaces).map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

/// The chat-id path-safety rule shared by the per-chat files (transcript,
/// History): uuid-shaped ids only — anything that could escape the file's
/// directory (path separators, dots) disables the file instead of being
/// sanitized into a colliding name.
pub(crate) fn chat_id_is_path_safe(chat_id: &str) -> bool {
    !chat_id.is_empty()
        && chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Per-chat transcript file.
pub(crate) fn transcript_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !chat_id_is_path_safe(chat_id) {
        return None;
    }
    Some(data_dir.join("transcripts").join(format!("{chat_id}.json")))
}

pub(crate) fn load_transcript(
    data_dir: &Path,
    chat_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    let Some(path) = transcript_path(data_dir, chat_id) else {
        return Ok(Vec::new());
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn persist_transcript(
    data_dir: &Path,
    chat_id: &str,
    transcript: &[SessionMessageEntry],
) -> Result<(), EngineError> {
    let Some(path) = transcript_path(data_dir, chat_id) else {
        return Ok(());
    };
    let dir = path.parent().expect("transcript path has a parent");
    std::fs::create_dir_all(dir)?;
    let temp_path = dir.join(format!(
        "{}.tmp",
        path.file_name().expect("file name").to_string_lossy()
    ));
    let bytes = serde_json::to_vec_pretty(transcript)
        .map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

/// Drop a chat's persisted transcript. Missing files are fine — chats that
/// never ran have nothing on disk.
pub(crate) fn delete_transcript(data_dir: &Path, chat_id: &str) {
    if let Some(path) = transcript_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
pub(crate) fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: holt_doc::MessageRole::User,
            parts: vec![],
            created_at: 42,
            device_id: "device".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn transcript_round_trips_and_unsafe_ids_are_ignored() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        persist_transcript(&dir, "chat-1", &[entry("m1")]).expect("persist");
        let loaded = load_transcript(&dir, "chat-1").expect("load");
        assert_eq!(loaded, vec![entry("m1")]);

        // Path-hostile ids neither read nor write outside the transcripts dir.
        persist_transcript(&dir, "../escape", &[entry("m2")]).expect("persist skipped");
        assert!(load_transcript(&dir, "../escape").unwrap().is_empty());
        assert!(!dir.join("transcripts").join("escape.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
