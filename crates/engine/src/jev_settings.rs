//! Engine-owned Jev settings (ADR-0026): the device-wide record of the
//! user's own TypeSafe API key behind Jev review, persisted as `jev.json`
//! under the credentials pattern — 0600 permissions, atomic replace +
//! sync, and a malformed file fails startup loudly (this record holds a
//! secret, so silent fallback is wrong; the file is left untouched for
//! manual repair). Like the search key, it is an independent record:
//! never shared with, or prefilled from, a same-vendor provider key.
//!
//! Mutated only through the typed RPC surface. An unconfigured record
//! leaves the Jev review tier unavailable — grayed in the picker, never
//! an error — and a chat left in the mode runs its Turns under
//! confirm-changes until a key returns.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use crate::EngineError;

const FILE_NAME: &str = "jev.json";

/// The persisted record: nothing is stored unconfigured — the file exists
/// only while the key is set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JevRecord {
    pub(crate) api_key: String,
}

#[derive(Clone)]
pub(crate) struct JevStore {
    path: PathBuf,
    record: Arc<RwLock<Option<JevRecord>>>,
}

impl JevStore {
    /// Loads the record. A missing file is the unconfigured state; a
    /// present but malformed file is a startup error naming the path.
    pub(crate) fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let record = match std::fs::metadata(&path) {
            Ok(metadata) => {
                ensure_private_permissions(&path, &metadata)?;
                let bytes = std::fs::read(&path)?;
                Some(serde_json::from_slice(&bytes).map_err(|error| {
                    EngineError::Other(format!(
                        "jev settings file {} is malformed; fix or remove it manually: {error}",
                        path.display()
                    ))
                })?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            record: Arc::new(RwLock::new(record)),
        })
    }

    pub(crate) fn get(&self) -> Option<JevRecord> {
        self.record
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Record the key. The key is trimmed; an empty key is refused before
    /// anything moves. The in-memory record moves first and rolls back if
    /// the file write fails.
    pub(crate) fn save(&self, api_key: &str) -> Result<(), EngineError> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(EngineError::Other("jev API key must not be empty".into()));
        }
        let next = JevRecord {
            api_key: api_key.to_string(),
        };
        let mut record = self
            .record
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = record.clone();
        *record = Some(next);
        if let Err(error) = self.persist(&record) {
            *record = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Clear the record — the unconfigured state is no file at all.
    pub(crate) fn remove(&self) -> Result<(), EngineError> {
        let mut record = self
            .record
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = record.clone();
        if previous.is_none() {
            return Ok(());
        }
        *record = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                *record = previous;
                Err(EngineError::Io(error))
            }
        }
    }

    fn persist(&self, record: &Option<JevRecord>) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("jev settings path has no parent".into()))?;
        let temp = parent.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(|error| EngineError::Other(format!("could not save jev settings: {error}")))
    }
}

#[cfg(unix)]
fn ensure_private_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                EngineError::Other(format!(
                    "could not secure jev settings file {}: {error}",
                    path.display()
                ))
            },
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_permissions(
    _path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), EngineError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_loads_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(JevStore::load(dir.path()).unwrap().get(), None);
    }

    #[test]
    fn saves_round_trip_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = JevStore::load(dir.path()).unwrap();
        store.save(" sk-123 ").unwrap();
        assert_eq!(
            store.get(),
            Some(JevRecord {
                api_key: "sk-123".into()
            })
        );
        assert_eq!(
            JevStore::load(dir.path()).unwrap().get(),
            Some(JevRecord {
                api_key: "sk-123".into()
            })
        );
        // The file carries camelCase fields, matching the RPC layer.
        assert!(
            std::fs::read_to_string(dir.path().join(FILE_NAME))
                .unwrap()
                .contains("\"apiKey\": \"sk-123\"")
        );
    }

    #[test]
    fn remove_clears_the_record_and_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = JevStore::load(dir.path()).unwrap();
        store.save("key").unwrap();
        store.remove().unwrap();
        assert_eq!(store.get(), None);
        assert!(!dir.path().join(FILE_NAME).exists());
        assert_eq!(JevStore::load(dir.path()).unwrap().get(), None);
        // Removing an unconfigured store is a no-op, never an error.
        store.remove().unwrap();
    }

    #[test]
    fn empty_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = JevStore::load(dir.path()).unwrap();
        assert!(store.save("   ").is_err());
        assert_eq!(store.get(), None);
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn a_failed_save_rolls_the_record_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = JevStore::load(dir.path()).unwrap();
        // A directory squatting on the record path makes the atomic write
        // fail; the in-memory record must not move.
        std::fs::create_dir_all(dir.path().join(FILE_NAME)).unwrap();
        assert!(store.save("second").is_err());
        assert_eq!(store.get(), None);
    }

    #[test]
    fn a_corrupt_file_fails_startup_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{broken").unwrap();
        assert!(JevStore::load(dir.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }

    #[cfg(unix)]
    #[test]
    fn creates_and_repairs_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = JevStore::load(dir.path()).unwrap();
        store.save("secret").unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(store);
        JevStore::load(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
