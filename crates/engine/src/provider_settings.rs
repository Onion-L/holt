use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};

use crate::EngineError;

const FILE_NAME: &str = "provider-settings.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSettings {
    #[serde(default)]
    custom_models: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone)]
pub struct ProviderSettingsStore {
    path: PathBuf,
    settings: Arc<RwLock<StoredSettings>>,
}

impl ProviderSettingsStore {
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let settings = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<StoredSettings>(&bytes).map_err(|error| {
                EngineError::Other(format!(
                    "provider settings file {} is malformed; fix or remove it manually: {error}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredSettings::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            settings: Arc::new(RwLock::new(settings)),
        })
    }

    pub fn custom_models_for(&self, provider_id: &str) -> Vec<String> {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .custom_models
            .get(provider_id)
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn add_custom_model(&self, provider_id: &str, model_id: &str) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        let changed = settings
            .custom_models
            .entry(provider_id.to_string())
            .or_default()
            .insert(model_id.to_string());
        if !changed {
            return Ok(());
        }
        if let Err(error) = self.persist(&settings) {
            *settings = previous;
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self, settings: &StoredSettings) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self.path.parent().ok_or_else(|| {
            EngineError::Other("provider settings path has no parent".to_string())
        })?;
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
    fn custom_models_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings.add_custom_model("openai", "gpt-custom").unwrap();
        settings.add_custom_model("openai", "gpt-custom").unwrap();

        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.custom_models_for("openai"), vec!["gpt-custom"]);
    }

    #[test]
    fn malformed_settings_fail_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{broken").unwrap();
        assert!(ProviderSettingsStore::load(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{broken");
    }
}
