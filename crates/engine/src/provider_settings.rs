use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use pi_core::ai::{compat, types::Model as CoreModel};
use serde::{Deserialize, Serialize};

use crate::{EngineError, provider_store};

const FILE_NAME: &str = "provider-settings.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredSettings {
    #[serde(default)]
    pub(crate) custom_models: BTreeMap<String, BTreeSet<String>>,
    /// Live, complete model records — the top catalog layer (ADR-0028). A
    /// same-id record replaces the catalog entry outright; ids new to the
    /// provider append.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) model_records: BTreeMap<String, BTreeMap<String, CoreModel>>,
    /// User-defined providers (id, name, baseUrl, default dialect); their
    /// models live in `model_records` under the same provider id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) custom_providers: BTreeMap<String, CustomProvider>,
    /// Catalog model ids excluded from listings; resolution keeps working.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) hidden_models: BTreeMap<String, BTreeSet<String>>,
}

/// A complete in-memory copy of the mutable provider catalog settings. Model
/// proposals use it as their optimistic-concurrency baseline so an approval
/// can never overwrite an intervening settings change.
pub(crate) type ProviderSettingsSnapshot = StoredSettings;

/// One user-defined provider: identity and transport only. Its models are
/// `model_records` entries under the same id, and its auth shape is api_key
/// by definition — keys enter through Settings, never this file. There is
/// deliberately no headers field: it never reached the request path, so
/// carrying it would only suggest a config that silently does nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Registered api dialect id; the default its model records build on.
    pub default_api: String,
}

/// The structural rules a custom provider entry must satisfy, shared by the
/// load-time drop and the write-time validation. Returns the drop reason.
pub(crate) fn custom_provider_problem(provider: &CustomProvider) -> Option<&'static str> {
    if provider.id.trim().is_empty() || provider.id.contains('/') {
        return Some("its id is blank or contains '/'");
    }
    if provider.name.trim().is_empty() {
        return Some("its name is blank");
    }
    if !provider_store::http_base_url(&provider.base_url) {
        return Some("its baseUrl is not http(s)");
    }
    if compat::get_api_provider(&provider.default_api).is_none() {
        return Some("its default api dialect is not registered");
    }
    None
}

#[derive(Clone)]
pub struct ProviderSettingsStore {
    path: PathBuf,
    settings: Arc<RwLock<StoredSettings>>,
}

impl ProviderSettingsStore {
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        // The file can carry model-record header values, so an existing
        // broader mode is tightened like the credential store's (a failed
        // tightening is logged, not fatal).
        if let Ok(metadata) = std::fs::metadata(&path) {
            ensure_private_permissions(&path, &metadata);
        }
        let settings = match std::fs::read(&path) {
            // A 0-byte file reads as "nothing ever written" (no in-repo path
            // produces one, but external tooling can truncate) — not as
            // corruption worth bricking the boot gate over.
            Ok(bytes) if bytes.is_empty() => StoredSettings::default(),
            Ok(bytes) => serde_json::from_slice::<StoredSettings>(&bytes).map_err(|error| {
                EngineError::Other(format!(
                    "provider settings file {} is malformed; fix or remove it manually: {error}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredSettings::default(),
            Err(error) => return Err(error.into()),
        };
        let mut settings = settings;
        sanitize(&mut settings);
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

    pub fn model_records_for(&self, provider_id: &str) -> Vec<CoreModel> {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .model_records
            .get(provider_id)
            .map(|records| records.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn custom_providers(&self) -> Vec<CustomProvider> {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .custom_providers
            .values()
            .cloned()
            .collect()
    }

    pub fn custom_provider(&self, provider_id: &str) -> Option<CustomProvider> {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .custom_providers
            .get(provider_id)
            .cloned()
    }

    pub fn hidden_models_for(&self, provider_id: &str) -> Vec<String> {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .hidden_models
            .get(provider_id)
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn snapshot(&self) -> ProviderSettingsSnapshot {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn replace_if_unchanged(
        &self,
        expected: &ProviderSettingsSnapshot,
        next: ProviderSettingsSnapshot,
    ) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if &*settings != expected {
            return Err(EngineError::Other(
                "provider settings changed since this proposal was created".into(),
            ));
        }
        let previous = settings.clone();
        *settings = next;
        self.commit(settings, previous)
    }

    /// Drops one user-added model id. Returns `false` when the id is not a
    /// custom model for this provider — builtin catalog rows live outside
    /// this store, so they can never be removed here.
    pub fn remove_custom_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool, EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        let removed = settings
            .custom_models
            .get_mut(provider_id)
            .is_some_and(|models| models.remove(model_id));
        if !removed {
            return Ok(false);
        }
        if settings
            .custom_models
            .get(provider_id)
            .is_some_and(|models| models.is_empty())
        {
            settings.custom_models.remove(provider_id);
        }
        if let Err(error) = self.persist(&settings) {
            *settings = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn upsert_model_record(
        &self,
        provider_id: &str,
        record: CoreModel,
    ) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        settings
            .model_records
            .entry(provider_id.to_string())
            .or_default()
            .insert(record.id.clone(), record);
        self.commit(settings, previous)
    }

    /// Drops one model record. Returns `false` when the provider carries no
    /// record under that id.
    pub fn remove_model_record(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool, EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        let removed = settings
            .model_records
            .get_mut(provider_id)
            .is_some_and(|records| records.remove(model_id).is_some());
        if !removed {
            return Ok(false);
        }
        if settings
            .model_records
            .get(provider_id)
            .is_some_and(|records| records.is_empty())
        {
            settings.model_records.remove(provider_id);
        }
        if let Some(hidden) = settings.hidden_models.get_mut(provider_id) {
            hidden.remove(model_id);
            if hidden.is_empty() {
                settings.hidden_models.remove(provider_id);
            }
        }
        self.commit(settings, previous)?;
        Ok(true)
    }

    pub fn upsert_custom_provider(&self, provider: CustomProvider) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        settings
            .custom_providers
            .insert(provider.id.clone(), provider);
        self.commit(settings, previous)
    }

    /// Drops a user-defined provider definition (its model records are
    /// separate entries and survive). Returns `false` for an unknown id.
    pub fn remove_custom_provider(&self, provider_id: &str) -> Result<bool, EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        let removed = settings.custom_providers.remove(provider_id).is_some();
        if !removed {
            return Ok(false);
        }
        self.commit(settings, previous)?;
        Ok(true)
    }

    /// Replaces the provider's hidden set wholesale; an empty set clears the
    /// hidden state entirely.
    pub fn set_hidden_models(
        &self,
        provider_id: &str,
        model_ids: BTreeSet<String>,
    ) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        if model_ids.is_empty() {
            settings.hidden_models.remove(provider_id);
        } else {
            settings
                .hidden_models
                .insert(provider_id.to_string(), model_ids);
        }
        self.commit(settings, previous)
    }

    /// Drops every user-written entry for one provider across all sections.
    /// Returns `false` when the provider carried nothing to reset.
    pub fn reset_provider(&self, provider_id: &str) -> Result<bool, EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        let removed = settings.custom_models.remove(provider_id).is_some()
            | settings.model_records.remove(provider_id).is_some()
            | settings.custom_providers.remove(provider_id).is_some()
            | settings.hidden_models.remove(provider_id).is_some();
        if !removed {
            return Ok(false);
        }
        self.commit(settings, previous)?;
        Ok(true)
    }

    /// Drops every user-written catalog entry for every provider. The
    /// compiled catalog under the hand-edited overlay is what remains
    /// (ADR-0028); credentials are not catalog entries and survive.
    pub fn reset_all(&self) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        *settings = StoredSettings::default();
        self.commit(settings, previous)
    }

    fn commit(
        &self,
        mut settings: std::sync::RwLockWriteGuard<'_, StoredSettings>,
        previous: StoredSettings,
    ) -> Result<(), EngineError> {
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
        result.map_err(EngineError::Io)
    }
}

/// On Unix the settings file carries the credential file's mode: it can
/// hold model-record header values, and it decides which host requests
/// (with the key) are sent to.
#[cfg(unix)]
fn ensure_private_permissions(path: &Path, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 == 0 {
        return;
    }
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            target: "holt::engine",
            %error,
            "could not tighten the provider settings file's permissions to 0600"
        );
    }
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path, _metadata: &std::fs::Metadata) {}

/// Applies the provider store's per-entry policy to the live sections: a
/// hand-edited entry that fails its checks is dropped with a log line while
/// the rest of the file still applies — an unparsable file remains the only
/// thing that fails the boot gate.
fn sanitize(settings: &mut StoredSettings) {
    settings.model_records.retain(|provider_id, records| {
        records.retain(|model_id, record| {
            match provider_store::model_record_problem(provider_id, record) {
                None => true,
                Some(reason) => {
                    tracing::warn!(
                        target: "holt::engine",
                        provider = provider_id,
                        model = model_id,
                        "dropping a provider-settings model record: {reason}"
                    );
                    false
                }
            }
        });
        !records.is_empty()
    });
    settings
        .custom_providers
        .retain(|id, provider| match custom_provider_problem(provider) {
            None => true,
            Some(reason) => {
                tracing::warn!(
                    target: "holt::engine",
                    provider = id,
                    "dropping a provider-settings custom provider: {reason}"
                );
                false
            }
        });
    settings.hidden_models.retain(|_, models| {
        models.retain(|model_id| !model_id.trim().is_empty());
        !models.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(provider: &str, id: &str, base_url: &str) -> CoreModel {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": base_url,
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
            "contextWindow": 128_000,
            "maxTokens": 8_192,
        }))
        .unwrap()
    }

    fn custom_provider(id: &str) -> CustomProvider {
        CustomProvider {
            id: id.to_string(),
            name: "Acme".to_string(),
            base_url: "https://acme.example/v1".to_string(),
            default_api: "openai-completions".to_string(),
        }
    }

    #[test]
    fn custom_models_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        // Bare custom ids have no write path left (the Add RPC is gone);
        // they arrive through legacy provider-settings.json files.
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom"] }
            }))
            .unwrap(),
        )
        .unwrap();

        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.custom_models_for("openai"), vec!["gpt-custom"]);
    }

    #[test]
    fn remove_custom_model_drops_only_custom_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom", "gpt-other"] }
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();

        // Unknown provider / builtin id: a no-op that reports false.
        assert!(
            !settings
                .remove_custom_model("anthropic", "claude-opus-5")
                .unwrap()
        );
        assert!(!settings.remove_custom_model("openai", "gpt-5").unwrap());

        assert!(
            settings
                .remove_custom_model("openai", "gpt-custom")
                .unwrap()
        );
        assert_eq!(
            settings.custom_models_for("openai"),
            vec!["gpt-other".to_string()]
        );
        assert!(settings.remove_custom_model("openai", "gpt-other").unwrap());
        assert!(settings.custom_models_for("openai").is_empty());

        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert!(restored.custom_models_for("openai").is_empty());
    }

    #[test]
    fn malformed_settings_fail_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{broken").unwrap();
        assert!(ProviderSettingsStore::load(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{broken");
    }

    #[test]
    fn empty_settings_file_loads_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"").unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        assert!(settings.custom_models_for("openai").is_empty());

        // The store keeps working: a later write persists over the empty file.
        let mut hidden = BTreeSet::new();
        hidden.insert("gpt-5.4".to_string());
        settings.set_hidden_models("openai", hidden).unwrap();
        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.hidden_models_for("openai"), vec!["gpt-5.4"]);
    }

    #[test]
    fn every_section_round_trips_and_legacy_files_stay_compatible() {
        // Bare custom models ride the file (their add path is gone); every
        // other section through the store's write API.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom"] }
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings
            .upsert_model_record(
                "openai",
                record("openai", "gpt-record", "https://openai.example/v1"),
            )
            .unwrap();
        settings
            .upsert_custom_provider(custom_provider("acme"))
            .unwrap();
        settings
            .upsert_model_record("acme", record("acme", "acme-1", "https://acme.example/v1"))
            .unwrap();
        let mut hidden = BTreeSet::new();
        hidden.insert("gpt-5.4".to_string());
        settings.set_hidden_models("openai", hidden).unwrap();

        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.custom_models_for("openai"), vec!["gpt-custom"]);
        assert_eq!(
            restored
                .model_records_for("openai")
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>(),
            vec!["gpt-record"]
        );
        assert_eq!(restored.custom_provider("acme").unwrap().name, "Acme");
        assert_eq!(
            restored
                .model_records_for("acme")
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>(),
            vec!["acme-1"]
        );
        assert_eq!(restored.hidden_models_for("openai"), vec!["gpt-5.4"]);

        // A legacy file (customModels only, no new fields) loads with the
        // new sections empty and keeps its old data through a later write.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom"] }
            }))
            .unwrap(),
        )
        .unwrap();
        let legacy = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(legacy.custom_models_for("openai"), vec!["gpt-custom"]);
        assert!(legacy.model_records_for("openai").is_empty());
        legacy
            .upsert_model_record(
                "openai",
                record("openai", "gpt-record", "https://openai.example/v1"),
            )
            .unwrap();
        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(restored.custom_models_for("openai"), vec!["gpt-custom"]);
        assert_eq!(restored.model_records_for("openai").len(), 1);
    }

    #[test]
    fn a_hand_edited_file_drops_bad_entries_but_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom", " "] },
                "modelRecords": {
                    "openai": {
                        "good": record("openai", "good", "https://openai.example/v1"),
                        "bad-url": record("openai", "bad-url", "not-a-url"),
                        "wrong-parent": record("anthropic", "wrong-parent", "https://openai.example/v1")
                    }
                },
                "customProviders": {
                    "good-gateway": custom_provider("good-gateway"),
                    "bad-gateway": {
                        "id": "bad-gateway",
                        "name": "Bad",
                        "baseUrl": "ftp://bad.example",
                        "defaultApi": "openai-completions"
                    }
                },
                "hiddenModels": { "openai": ["gpt-5.4", ""] }
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        // Blank legacy ids stay: the loader keeps the section whole.
        assert_eq!(settings.custom_models_for("openai").len(), 2);
        let records = settings.model_records_for("openai");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "good");
        assert!(settings.custom_provider("good-gateway").is_some());
        assert!(settings.custom_provider("bad-gateway").is_none());
        assert_eq!(settings.hidden_models_for("openai"), vec!["gpt-5.4"]);
    }

    #[test]
    fn reset_provider_drops_every_section_and_reset_all_clears_everything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "customModels": { "openai": ["gpt-custom"] }
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings
            .upsert_model_record(
                "openai",
                record("openai", "gpt-record", "https://openai.example/v1"),
            )
            .unwrap();
        let mut hidden = BTreeSet::new();
        hidden.insert("gpt-5.4".to_string());
        settings.set_hidden_models("openai", hidden).unwrap();
        settings
            .upsert_custom_provider(custom_provider("acme"))
            .unwrap();

        assert!(!settings.reset_provider("anthropic").unwrap());
        assert!(settings.reset_provider("openai").unwrap());
        assert!(settings.custom_models_for("openai").is_empty());
        assert!(settings.model_records_for("openai").is_empty());
        assert!(settings.hidden_models_for("openai").is_empty());
        // Other providers are untouched.
        assert!(settings.custom_provider("acme").is_some());

        settings.reset_all().unwrap();
        assert!(settings.custom_provider("acme").is_none());
        let restored = ProviderSettingsStore::load(dir.path()).unwrap();
        assert!(restored.custom_providers().is_empty());
    }

    #[test]
    fn removing_records_and_providers_prunes_empty_sections() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings
            .upsert_model_record("acme", record("acme", "acme-1", "https://acme.example/v1"))
            .unwrap();
        assert!(settings.remove_model_record("acme", "acme-1").unwrap());
        assert!(!settings.remove_model_record("acme", "acme-1").unwrap());
        assert!(settings.model_records_for("acme").is_empty());

        settings
            .upsert_custom_provider(custom_provider("acme"))
            .unwrap();
        assert!(settings.remove_custom_provider("acme").unwrap());
        assert!(!settings.remove_custom_provider("acme").unwrap());

        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(FILE_NAME)).unwrap()).unwrap();
        assert!(document.get("modelRecords").is_none());
        assert!(document.get("customProviders").is_none());
        assert!(document.get("hiddenModels").is_none());
        // The legacy section keeps its always-present shape: an empty map,
        // exactly what the pre-records code wrote.
        assert_eq!(document["customModels"], serde_json::json!({}));
    }

    #[test]
    fn removing_a_record_also_prunes_its_hidden_id() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings
            .upsert_model_record("acme", record("acme", "acme-1", "https://acme.example/v1"))
            .unwrap();
        settings
            .set_hidden_models("acme", BTreeSet::from(["acme-1".to_string()]))
            .unwrap();
        assert!(settings.remove_model_record("acme", "acme-1").unwrap());
        assert!(settings.hidden_models_for("acme").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_settings_file_is_private_and_a_broader_mode_is_tightened_on_load() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = ProviderSettingsStore::load(dir.path()).unwrap();
        settings
            .upsert_model_record("acme", record("acme", "acme-1", "https://acme.example/v1"))
            .unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(settings);
        ProviderSettingsStore::load(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
