//! JSON persistence for spaces/chats and the stable device id, written
//! atomically (tmp + rename) under the data dir.

use std::path::{Path, PathBuf};

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
