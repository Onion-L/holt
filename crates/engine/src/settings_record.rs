//! One JSON settings record in the data dir — the store shape the
//! engine-owned settings (title task, goal verifier) share. Mutated only
//! through the typed RPC surface; a missing, empty, or corrupt file loads
//! as the defaults, so an optional feature's record can never brick engine
//! boot, and the corrupt file is left untouched (the next save repairs it).

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use serde::{Serialize, de::DeserializeOwned};

use crate::EngineError;

#[derive(Clone)]
pub(crate) struct SettingsRecord<T> {
    path: PathBuf,
    settings: Arc<RwLock<T>>,
}

impl<T> SettingsRecord<T>
where
    T: Clone + Default + Serialize + DeserializeOwned,
{
    pub(crate) fn load(data_dir: &Path, file_name: &str) -> Result<Self, EngineError> {
        let path = data_dir.join(file_name);
        let settings = match std::fs::read(&path) {
            Ok(bytes) if bytes.is_empty() => T::default(),
            Ok(bytes) => serde_json::from_slice::<T>(&bytes).unwrap_or_else(|error| {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "settings record is corrupt; falling back to defaults"
                );
                T::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => T::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            settings: Arc::new(RwLock::new(settings)),
        })
    }

    pub(crate) fn get(&self) -> T {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Swap the record in memory, then persist atomically (temp file +
    /// rename); a failed persist rolls the in-memory value back.
    pub(crate) fn save(&self, next: T) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        *settings = next;
        if let Err(error) = self.persist(&settings) {
            *settings = previous;
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self, settings: &T) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("settings record path has no parent".into()))?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record");
        let temp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
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
