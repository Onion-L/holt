//! Engine-owned web-search settings (ADR-0023): the device-wide list of
//! configured search backends — built-in vendors and MCP-served search
//! tools — plus which one is active, persisted as
//! `web-search.json` under the credentials pattern — 0600 permissions,
//! atomic replace + sync, and a malformed file fails startup loudly (this
//! record holds secrets, so silent fallback is wrong; the file is left
//! untouched for manual repair). Search keys are independent records:
//! never shared with, or prefilled from, a same-vendor provider key.
//!
//! A built-in entry's id is its kind (one entry per vendor); an MCP entry
//! gets a generated `mcp-…` id. The pre-list single-record file
//! (`{backend, apiKey}`) still loads, as one active entry, and is
//! rewritten in the list shape on the next change.
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

const FILE_NAME: &str = "web-search.json";
/// The kind of an entry served by a tool of an `mcp.json` server.
pub(crate) const MCP_KIND: &str = "mcp";

/// One configured backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebSearchEntry {
    pub(crate) id: String,
    /// A built-in backend id, or [`MCP_KIND`].
    pub(crate) kind: String,
    /// MCP entries only: the `mcp.json` server name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server: Option<String>,
    /// MCP entries only: the server's search tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool: Option<String>,
    /// Empty for an MCP entry — the server's own config carries its auth.
    #[serde(default)]
    pub(crate) api_key: String,
}

/// The persisted record. Nothing is stored while the list is empty — the
/// file exists only while at least one entry does.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebSearchSettings {
    /// The entry the next Turn mounts; `None` leaves web search off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active: Option<String>,
    pub(crate) entries: Vec<WebSearchEntry>,
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
                entries: vec![WebSearchEntry {
                    id: legacy.backend.clone(),
                    kind: legacy.backend,
                    server: None,
                    tool: None,
                    api_key: legacy.api_key,
                }],
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
    /// Loads the record. A missing file is the unconfigured state; a
    /// present but malformed file is a startup error naming the path.
    pub(crate) fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let settings = match std::fs::metadata(&path) {
            Ok(metadata) => {
                ensure_private_permissions(&path, &metadata)?;
                let bytes = std::fs::read(&path)?;
                serde_json::from_slice::<OnDisk>(&bytes)
                    .map_err(|error| {
                        EngineError::Other(format!(
                            "web-search settings file {} is malformed; fix or remove it manually: {error}",
                            path.display()
                        ))
                    })?
                    .into()
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

    /// Insert or replace the entry with `entry.id` and make it active. A
    /// entry with an empty id is new and gets a generated one; the
    /// saved id is returned. Validation is the caller's; the key is
    /// trimmed here.
    pub(crate) fn save(&self, mut entry: WebSearchEntry) -> Result<String, EngineError> {
        entry.api_key = entry.api_key.trim().to_string();
        if entry.id.is_empty() {
            let uuid = uuid::Uuid::new_v4().simple().to_string();
            entry.id = format!("{MCP_KIND}-{}", &uuid[..8]);
        }
        let id = entry.id.clone();
        self.update(|settings| {
            match settings.entries.iter_mut().find(|slot| slot.id == id) {
                Some(slot) => *slot = entry,
                None => settings.entries.push(entry),
            }
            settings.active = Some(id.clone());
        })?;
        Ok(id)
    }

    /// Point the next Turn at an existing entry. An unknown id is refused.
    pub(crate) fn set_active(&self, id: &str) -> Result<(), EngineError> {
        if self.get().entry(id).is_none() {
            return Err(EngineError::Other(format!(
                "no search backend with id {id:?}"
            )));
        }
        self.update(|settings| settings.active = Some(id.to_string()))
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
    /// record rolls back if the file write fails. An empty list is stored
    /// as no file at all.
    fn update(&self, change: impl FnOnce(&mut WebSearchSettings)) -> Result<(), EngineError> {
        let mut settings = self
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = settings.clone();
        change(&mut settings);
        let result = if settings.entries.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(EngineError::Io(error)),
            }
        } else {
            self.persist(&settings)
        };
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
        WebSearchEntry {
            id: kind.into(),
            kind: kind.into(),
            server: None,
            tool: None,
            api_key: api_key.into(),
        }
    }

    fn mcp(server: &str, tool: &str) -> WebSearchEntry {
        WebSearchEntry {
            id: String::new(),
            kind: MCP_KIND.into(),
            server: Some(server.into()),
            tool: Some(tool.into()),
            api_key: String::new(),
        }
    }

    #[test]
    fn a_missing_file_loads_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            WebSearchStore::load(dir.path()).unwrap().get(),
            WebSearchSettings::default()
        );
    }

    #[test]
    fn saves_round_trip_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        assert_eq!(store.save(builtin("zhipu", " sk-123 ")).unwrap(), "zhipu");
        let settings = store.get();
        assert_eq!(settings.active_entry(), Some(&builtin("zhipu", "sk-123")));
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
        let id = store.save(mcp("tinyfish", "search")).unwrap();
        assert!(id.starts_with("mcp-"), "unexpected id {id}");
        assert_eq!(store.get().active.as_deref(), Some(id.as_str()));

        store.save(builtin("zhipu", "two")).unwrap();
        let settings = store.get();
        assert_eq!(settings.entries.len(), 2);
        assert_eq!(settings.active_entry(), Some(&builtin("zhipu", "two")));

        let mut retooled = mcp("tinyfish", "web_search");
        retooled.id = id.clone();
        assert_eq!(store.save(retooled).unwrap(), id);
        let settings = store.get();
        assert_eq!(settings.entries.len(), 2);
        assert_eq!(
            settings.entry(&id).unwrap().tool.as_deref(),
            Some("web_search")
        );
    }

    #[test]
    fn set_active_switches_between_entries_and_refuses_unknown_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "one")).unwrap();
        store.save(builtin("brave", "two")).unwrap();
        store.set_active("zhipu").unwrap();
        assert_eq!(
            WebSearchStore::load(dir.path())
                .unwrap()
                .get()
                .active
                .as_deref(),
            Some("zhipu")
        );
        assert!(store.set_active("bocha").is_err());
        assert_eq!(store.get().active.as_deref(), Some("zhipu"));
    }

    #[test]
    fn removing_entries_clears_active_and_finally_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "one")).unwrap();
        store.save(builtin("brave", "two")).unwrap();

        store.remove("zhipu").unwrap();
        assert_eq!(store.get().active.as_deref(), Some("brave"));
        store.remove("brave").unwrap();
        assert_eq!(store.get(), WebSearchSettings::default());
        assert!(!dir.path().join(FILE_NAME).exists());
        // Removing an unknown id is a no-op, never an error.
        store.remove("brave").unwrap();
    }

    #[test]
    fn removing_the_active_entry_turns_search_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebSearchStore::load(dir.path()).unwrap();
        store.save(builtin("zhipu", "one")).unwrap();
        store.save(builtin("brave", "two")).unwrap();
        store.remove("brave").unwrap();
        let settings = store.get();
        assert_eq!(settings.active, None);
        assert_eq!(settings.entries, vec![builtin("zhipu", "one")]);
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
