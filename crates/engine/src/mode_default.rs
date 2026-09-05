//! Engine-owned sticky permission-mode default (ADR-0014): one JSON record
//! in the data dir, mutated only through the mode RPC. New chats inherit
//! the last mode chosen on the device; the very first launch defaults to
//! confirm-changes. Same pattern as the title-settings record: a missing,
//! empty, or corrupt file loads as the default (this record backs a
//! preference, so it must never brick engine boot) and the corrupt file is
//! left untouched.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use holt_proto::PermissionMode;

use crate::EngineError;

const FILE_NAME: &str = "permission-mode-default.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    mode: PermissionMode,
}

#[derive(Clone)]
pub(crate) struct ModeDefaultStore {
    path: PathBuf,
    state: Arc<RwLock<PermissionMode>>,
}

impl ModeDefaultStore {
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let mode = match std::fs::read(&path) {
            Ok(bytes) if bytes.is_empty() => PermissionMode::default(),
            Ok(bytes) => serde_json::from_slice::<Record>(&bytes)
                .map(|record| record.mode)
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        error = %error,
                        "permission-mode default file is corrupt; falling back to confirm-changes"
                    );
                    PermissionMode::default()
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PermissionMode::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            state: Arc::new(RwLock::new(mode)),
        })
    }

    pub fn get(&self) -> PermissionMode {
        *self.state.read().unwrap_or_else(|error| error.into_inner())
    }

    /// Record the chosen mode as the device default for new chats. The
    /// in-memory value moves first and rolls back if the file write fails,
    /// so readers never see a default the disk cannot restore.
    pub fn save(&self, mode: PermissionMode) -> Result<(), EngineError> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = *state;
        *state = mode;
        if let Err(error) = self.persist(mode) {
            *state = previous;
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self, mode: PermissionMode) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(&Record { mode })
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("mode-default path has no parent".into()))?;
        let temp = parent.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(EngineError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_record_loads_confirm_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModeDefaultStore::load(dir.path()).unwrap();
        assert_eq!(store.get(), PermissionMode::ConfirmChanges);
    }

    #[test]
    fn chosen_mode_round_trips_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModeDefaultStore::load(dir.path()).unwrap();
        store.save(PermissionMode::FullAccess).unwrap();
        let restored = ModeDefaultStore::load(dir.path()).unwrap();
        assert_eq!(restored.get(), PermissionMode::FullAccess);
    }

    #[test]
    fn corrupt_record_loads_default_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{broken").unwrap();
        let store = ModeDefaultStore::load(dir.path()).unwrap();
        assert_eq!(store.get(), PermissionMode::ConfirmChanges);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        // The store keeps working: a later save repairs the file.
        store.save(PermissionMode::AutoReview).unwrap();
        assert_eq!(
            ModeDefaultStore::load(dir.path()).unwrap().get(),
            PermissionMode::AutoReview
        );
    }

    #[test]
    fn a_failed_save_rolls_the_in_memory_default_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModeDefaultStore::load(dir.path()).unwrap();
        // A directory squatting on the record path makes the atomic write
        // fail; the default must not move.
        std::fs::create_dir_all(dir.path().join(FILE_NAME)).unwrap();
        assert!(store.save(PermissionMode::FullAccess).is_err());
        assert_eq!(store.get(), PermissionMode::ConfirmChanges);
    }
}
