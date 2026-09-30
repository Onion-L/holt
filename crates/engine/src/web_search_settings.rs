//! Engine-owned web-search settings (ADR-0023): the device-wide list of
//! configured search backends plus which one is active,
//! persisted as `web-search.json` under the credentials pattern — 0600
//! permissions, atomic replace + sync, and a malformed file fails startup
//! loudly (this record holds secrets, so silent fallback is wrong; the
//! file is left untouched for manual repair). Search keys are independent
//! records: never shared with, or prefilled from, a same-vendor provider
//! key.
//!
//! An entry's id is its kind (one entry per vendor). With no file at all
//! the keyless default backend is active, so search works before any
//! setup; once the user changes anything the file records their choice,
//! including "off". An entry of a kind nothing offers — a custom
//! definition removed from, or broken in, `search-backends.json` — stays
//! (key included, so fixing the definition brings it back) and mounts
//! nothing. The pre-list single-record file (`{backend, apiKey}`)
//! still loads, as one active entry, and is rewritten in the list shape
//! on the next change.
//!
//! Mutated only through the typed RPC surface (which owns validation).
//! Resolution into a mounted backend happens once per Turn admission in
//! the engine, through the adapter table in `tools::web_search` — no
//! active entry mounts no `web_search` tool at all.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use crate::EngineError;
use crate::tools::web_search::DEFAULT_BACKEND;

const FILE_NAME: &str = "web-search.json";

/// One configured backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebSearchEntry {
    pub(crate) id: String,
    /// A built-in backend id or a custom definition's.
    pub(crate) kind: String,
    /// Empty for a keyless backend.
    #[serde(default)]
    pub(crate) api_key: String,
}

impl WebSearchEntry {
    pub(crate) fn new(kind: &str, api_key: String) -> Self {
        Self {
            id: kind.to_string(),
            kind: kind.to_string(),
            api_key,
        }
    }
}

/// The persisted record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebSearchSettings {
    /// The entry the next Turn mounts; `None` leaves web search off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active: Option<String>,
    /// Required, so a legacy `{backend, apiKey}` record never decodes as
    /// an empty list.
    pub(crate) entries: Vec<WebSearchEntry>,
}

impl Default for WebSearchSettings {
    /// A fresh install: the keyless default backend, active.
    fn default() -> Self {
        Self {
            active: Some(DEFAULT_BACKEND.to_string()),
            entries: vec![WebSearchEntry::new(DEFAULT_BACKEND, String::new())],
        }
    }
}

impl WebSearchSettings {
    pub(crate) fn entry(&self, id: &str) -> Option<&WebSearchEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub(crate) fn active_entry(&self) -> Option<&WebSearchEntry> {
        self.entry(self.active.as_deref()?)
    }
}

/// The on-disk shapes `load` accepts: the current list, or the pre-list
/// single record.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OnDisk {
    Current(WebSearchSettings),
    Legacy(LegacyRecord),
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRecord {
    backend: String,
    api_key: String,
}

impl From<OnDisk> for WebSearchSettings {
    fn from(on_disk: OnDisk) -> Self {
        match on_disk {
            OnDisk::Current(settings) => settings,
            OnDisk::Legacy(legacy) => Self {
                active: Some(legacy.backend.clone()),
                entries: vec![WebSearchEntry::new(&legacy.backend, legacy.api_key)],
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct WebSearchStore {
    path: PathBuf,
    settings: Arc<RwLock<WebSearchSettings>>,
}

impl WebSearchStore {
    /// Loads the record. A missing file is the fresh-install default; a
    /// present but malformed file is a startup error naming the path.
    pub(crate) fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let settings = match std::fs::metadata(&path) {
            Ok(metadata) => {
                ensure_private_permissions(&path, &metadata)?;
                let bytes = std::fs::read(&path)?;
                let on_disk = serde_json::from_slice::<OnDisk>(&bytes)
                    .map_err(|error| {
                        EngineError::Other(format!(
                            "web-search settings file {} is malformed; fix or remove it manually: {error}",
                            path.display()
                        ))
                    })?;
                WebSearchSettings::from(on_disk)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                WebSearchSettings::default()
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            settings: Arc::new(RwLock::new(settings)),
        })
    }

    pub(crate) fn get(&self) -> WebSearchSettings {
        self.settings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Insert or replace the entry with `entry.id` and make it active.
    /// Validation is the caller's; the key is trimmed here.
    pub(crate) fn save(&self, mut entry: WebSearchEntry) -> Result<(), EngineError> {
        entry.api_key = entry.api_key.trim().to_string();
        let id = entry.id.clone();
        self.update(|settings| {
            match settings.entries.iter_mut().find(|slot| slot.id == id) {
                Some(slot) => *slot = entry,
                None => settings.entries.push(entry),
            }
            settings.active = Some(id);
        })
    }

    /// Point the next Turn at an existing entry, or at none (web search
    /// off, entries kept). An unknown id is refused.
    pub(crate) fn set_active(&self, id: Option<&str>) -> Result<(), EngineError> {
        if let Some(id) = id
            && self.get().entry(id).is_none()
        {
            return Err(EngineError::Other(format!(
                "no search backend with id {id:?}"
            )));
        }
        self.update(|settings| settings.active = id.map(str::to_string))
    }

    /// Drop one entry; removing the active one leaves web search off. An
    /// unknown id is a no-op.
    pub(crate) fn remove(&self, id: &str) -> Result<(), EngineError> {
        if self.get().entry(id).is_none() {
            return Ok(());
        }
        self.update(|settings| {
            settings.entries.retain(|entry| entry.id != id);
            if settings.active.as_deref() == Some(id) {
                settings.active = None;
            }
        })
    }

    /// Apply `change` to the in-memory record, then persist it; the
    /// record rolls back if the file write fails.
    fn update(&self, change: impl FnOnce(&mut WebSearchSettings)) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        change(&mut settings);
        let result = self.persist(&settings);
        if result.is_err() {
            *settings = previous;
        }
        result
    }

    fn persist(&self, settings: &WebSearchSettings) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| EngineError::Other("web-search settings path has no parent".into()))?;
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
        result.map_err(|error| {
            EngineError::Other(format!("could not save web-search settings: {error}"))
        })
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
                    "could not secure web-search settings file {}: {error}",
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

    fn builtin(kind: &str, api_key: &str) -> WebSearchEntry {
        WebSearchEntry::new(kind, api_key.into())
    }

    fn exa() -> WebSearchEntry {
        builtin("exa", "")
    }

    #[test]
    fn a_missing_file_loads_the_keyless_default_active() {
        let dir = tempfile::tempdir().unwrap();
        let settings = WebSearchStore::load(dir.path()).unwrap().get();
        assert_eq!(settings.active_entry(), Some(&exa()));
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn saves_round_trip_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", " sk-123 ")).unwrap();
        let settings = store.get();
        assert_eq!(settings.active_entry(), Some(&builtin("zhipu", "sk-123")));
        assert_eq!(settings.entries, vec![exa(), builtin("zhipu", "sk-123")]);
        assert_eq!(WebSearchStore::load(dir.path()).unwrap().get(), settings);
        // The file carries camelCase fields, matching the RPC layer.
        assert!(
            std::fs::read_to_string(dir.path().join(FILE_NAME))
                .unwrap()
                .contains("\"apiKey\": \"sk-123\"")
        );
    }

    #[test]
    fn saving_activates_and_updates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "one")).unwrap();
        store.save(builtin("brave", "two")).unwrap();
        store.save(builtin("zhipu", "three")).unwrap();
        let settings = store.get();
        assert_eq!(settings.entries.len(), 3);
        assert_eq!(settings.active_entry(), Some(&builtin("zhipu", "three")));
    }

    #[test]
    fn set_active_switches_turns_off_and_refuses_unknown_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "one")).unwrap();
        store.set_active(Some("exa")).unwrap();
        assert_eq!(
            WebSearchStore::load(dir.path())
                .unwrap()
                .get()
                .active
                .as_deref(),
            Some("exa")
        );
        assert!(store.set_active(Some("bocha")).is_err());
        assert_eq!(store.get().active.as_deref(), Some("exa"));

        // Off persists — a reload does not fall back to the default.
        store.set_active(None).unwrap();
        let reloaded = WebSearchStore::load(dir.path()).unwrap().get();
        assert_eq!(reloaded.active, None);
        assert_eq!(reloaded.entries.len(), 2);
    }

    #[test]
    fn removing_the_active_entry_turns_search_off_and_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("brave", "two")).unwrap();
        store.remove("brave").unwrap();
        store.remove("exa").unwrap();
        assert_eq!(
            WebSearchStore::load(dir.path()).unwrap().get(),
            WebSearchSettings {
                active: None,
                entries: Vec::new(),
            }
        );
        // Removing an unknown id is a no-op, never an error.
        store.remove("brave").unwrap();
    }

    #[test]
    fn entries_of_unknown_kinds_survive_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"active": "tavily", "entries": [
                {"id": "tavily", "kind": "tavily", "apiKey": "sk-t"},
                {"id": "brave", "kind": "brave", "apiKey": "sk"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            WebSearchStore::load(dir.path()).unwrap().get(),
            WebSearchSettings {
                active: Some("tavily".into()),
                entries: vec![builtin("tavily", "sk-t"), builtin("brave", "sk")],
            }
        );
    }

    #[test]
    fn a_legacy_single_record_loads_as_one_active_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"backend": "bocha", "apiKey": "sk-old"}"#,
        )
        .unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        assert_eq!(
            store.get(),
            WebSearchSettings {
                active: Some("bocha".into()),
                entries: vec![builtin("bocha", "sk-old")],
            }
        );
        // The next change rewrites the file in the list shape.
        store.save(builtin("brave", "sk-new")).unwrap();
        let reloaded = WebSearchStore::load(dir.path()).unwrap().get();
        assert_eq!(reloaded.entries.len(), 2);
        assert!(
            std::fs::read_to_string(dir.path().join(FILE_NAME))
                .unwrap()
                .contains("\"entries\"")
        );
    }

    #[test]
    fn a_failed_save_rolls_the_record_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        // A directory squatting on the record path makes the atomic write
        // fail; the in-memory record must not move.
        std::fs::create_dir_all(dir.path().join(FILE_NAME)).unwrap();
        assert!(store.save(builtin("bocha", "second")).is_err());
        assert_eq!(store.get(), WebSearchSettings::default());
    }

    #[test]
    fn a_corrupt_file_fails_startup_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{broken").unwrap();
        assert!(WebSearchStore::load(dir.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }

    #[cfg(unix)]
    #[test]
    fn creates_and_repairs_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "secret")).unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(store);
        WebSearchStore::load(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
