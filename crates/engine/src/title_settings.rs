//! Engine-owned title-task settings (ADR-0012): one JSON record in the data
//! dir, separate from UI settings, mutated only through the typed RPC
//! surface. An empty model id means automatic titles are disabled.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use holt_proto::TitleSettings;

use crate::EngineError;

const FILE_NAME: &str = "title-settings.json";

/// Upper bound for a saved instruction — generous enough for tone/language
/// customization, small enough to catch a pasted document.
pub(crate) const MAX_TITLE_INSTRUCTION_CHARS: usize = 2000;

#[derive(Clone)]
pub struct TitleSettingsStore {
    path: PathBuf,
    settings: Arc<RwLock<TitleSettings>>,
}

impl TitleSettingsStore {
    /// Loads the record. Unlike credentials or provider settings, a missing,
    /// empty, or corrupt file loads as the defaults (automatic titles
    /// disabled): this record backs an optional enhancement, so it must
    /// never brick engine boot, and the corrupt file is left untouched.
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let settings = match std::fs::read(&path) {
            Ok(bytes) if bytes.is_empty() => TitleSettings::default(),
            Ok(bytes) => serde_json::from_slice::<TitleSettings>(&bytes).unwrap_or_else(|error| {
                tracing::warn!(
                    error = %error,
                    "title settings file is corrupt; falling back to defaults"
                );
                TitleSettings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TitleSettings::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            settings: Arc::new(RwLock::new(settings)),
        })
    }

    pub fn get(&self) -> TitleSettings {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn save(&self, next: TitleSettings) -> Result<(), EngineError> {
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

    fn persist(&self, settings: &TitleSettings) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("title settings path has no parent".to_string()))?;
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

    fn settings(model_id: Option<&str>, instruction: &str) -> TitleSettings {
        TitleSettings {
            model_id: model_id.map(str::to_string),
            instruction: instruction.to_string(),
        }
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());

        store
            .save(settings(Some("openai/gpt-5.4"), "name it"))
            .unwrap();
        let restored = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.get(), settings(Some("openai/gpt-5.4"), "name it"));
    }

    #[test]
    fn corrupt_settings_load_as_defaults_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{broken").unwrap();
        let store = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");

        // The store keeps working: a later save repairs the file.
        store.save(settings(None, "name it")).unwrap();
        let restored = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.get(), settings(None, "name it"));
    }

    #[test]
    fn empty_settings_file_loads_as_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"").unwrap();
        let store = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());
    }

    #[test]
    fn a_record_missing_new_fields_fills_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"modelId":"openai/gpt-5.4"}"#,
        )
        .unwrap();
        let store = TitleSettingsStore::load(dir.path()).unwrap();
        assert_eq!(
            store.get(),
            settings(
                Some("openai/gpt-5.4"),
                holt_proto::DEFAULT_TITLE_INSTRUCTION
            )
        );
    }
}
