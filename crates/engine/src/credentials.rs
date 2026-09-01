use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use pi_core::ai::auth::types::{
    ApiKeyCredential, AuthFuture, AuthOperationOptions, AuthStorageError, BoxedAuthError,
    Credential, CredentialInfo, CredentialStore, ModifyFn,
};
use tokio::sync::Mutex;

use crate::EngineError;

const FILE_NAME: &str = "provider-credentials.json";

#[derive(Clone)]
pub struct HoltCredentialStore {
    path: PathBuf,
    entries: Arc<Mutex<BTreeMap<String, String>>>,
}

impl HoltCredentialStore {
    pub fn load(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join(FILE_NAME);
        let entries = match std::fs::metadata(&path) {
            Ok(metadata) => {
                ensure_private_permissions(&path, &metadata)?;
                let bytes = std::fs::read(&path)?;
                serde_json::from_slice(&bytes).map_err(|error| {
                    EngineError::Other(format!(
                        "credential file {} is malformed; fix or remove it manually: {error}",
                        path.display()
                    ))
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    fn persist(&self, entries: &BTreeMap<String, String>) -> Result<(), AuthStorageError> {
        let bytes = serde_json::to_vec_pretty(entries)
            .map_err(|error| AuthStorageError(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| AuthStorageError("credential path has no parent".into()))?;
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
        result.map_err(|error| AuthStorageError(format!("could not save credentials: {error}")))
    }

    pub async fn save_key(&self, provider_id: &str, key: &str) -> Result<(), AuthStorageError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(AuthStorageError("API key must not be empty".into()));
        }
        let mut entries = self.entries.lock().await;
        let previous = entries.insert(provider_id.to_string(), key.to_string());
        if let Err(error) = self.persist(&entries) {
            match previous {
                Some(value) => {
                    entries.insert(provider_id.to_string(), value);
                }
                None => {
                    entries.remove(provider_id);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub async fn reveal_key(&self, provider_id: &str) -> Option<String> {
        self.entries.lock().await.get(provider_id).cloned()
    }

    pub async fn configured_ids(&self) -> Vec<String> {
        self.entries.lock().await.keys().cloned().collect()
    }
}

impl CredentialStore for HoltCredentialStore {
    fn read(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>> {
        let this = self.clone();
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            Ok(this
                .entries
                .lock()
                .await
                .get(&provider_id)
                .cloned()
                .map(|key| {
                    Credential::ApiKey(ApiKeyCredential {
                        key: Some(key),
                        env: None,
                    })
                }))
        })
    }

    fn list(
        &self,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>> {
        let this = self.clone();
        Box::pin(async move {
            Ok(this
                .entries
                .lock()
                .await
                .keys()
                .map(|provider_id| CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: "api_key".into(),
                })
                .collect())
        })
    }

    fn modify(
        &self,
        provider_id: &str,
        modify: ModifyFn,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>> {
        let this = self.clone();
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            let storage_error = |message: &str| -> BoxedAuthError {
                Box::new(AuthStorageError(message.to_string()))
            };
            let mut entries = this.entries.lock().await;
            let current = entries.get(&provider_id).cloned().map(|key| {
                Credential::ApiKey(ApiKeyCredential {
                    key: Some(key),
                    env: None,
                })
            });
            let next = modify(current).await?;
            if let Some(Credential::ApiKey(value)) = &next {
                let key = value.key.as_deref().unwrap_or_default().trim();
                if key.is_empty() {
                    return Err(storage_error("API key must not be empty"));
                }
                let previous = entries.insert(provider_id.clone(), key.to_string());
                if let Err(error) = this.persist(&entries) {
                    match previous {
                        Some(value) => {
                            entries.insert(provider_id, value);
                        }
                        None => {
                            entries.remove(&provider_id);
                        }
                    }
                    return Err(Box::new(error) as BoxedAuthError);
                }
            }
            Ok(next)
        })
    }

    fn delete(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>> {
        let this = self.clone();
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            let mut entries = this.entries.lock().await;
            let previous = entries.remove(&provider_id);
            if previous.is_none() {
                return Ok(());
            }
            if entries.is_empty() {
                match std::fs::remove_file(&this.path) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => {
                        entries.insert(provider_id, previous.unwrap());
                        return Err(AuthStorageError(format!(
                            "could not remove credentials: {error}"
                        )));
                    }
                }
            }
            if let Err(error) = this.persist(&entries) {
                entries.insert(provider_id, previous.unwrap());
                return Err(error);
            }
            Ok(())
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
                    "could not secure credential file {}: {error}",
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

    #[tokio::test]
    async fn persists_independent_keys_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let store = HoltCredentialStore::load(dir.path()).unwrap();
        store.save_key("openai", " first ").await.unwrap();
        store.save_key("anthropic", "second").await.unwrap();
        store.save_key("openai", "replacement").await.unwrap();

        let active_snapshot = store.read("openai", None).await.unwrap().unwrap();
        store.delete("openai", None).await.unwrap();
        assert!(store.read("openai", None).await.unwrap().is_none());
        assert_eq!(
            store.reveal_key("anthropic").await.as_deref(),
            Some("second")
        );
        assert!(
            matches!(active_snapshot, Credential::ApiKey(ApiKeyCredential { key: Some(key), .. }) if key == "replacement")
        );

        let reloaded = HoltCredentialStore::load(dir.path()).unwrap();
        assert_eq!(
            reloaded.reveal_key("anthropic").await.as_deref(),
            Some("second")
        );
    }

    #[tokio::test]
    async fn rejects_empty_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = HoltCredentialStore::load(dir.path()).unwrap();
        assert!(store.save_key("openai", "  ").await.is_err());
    }

    #[test]
    fn corrupt_file_fails_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{broken").unwrap();
        assert!(HoltCredentialStore::load(dir.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn creates_and_repairs_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = HoltCredentialStore::load(dir.path()).unwrap();
        store.save_key("openai", "secret").await.unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(store);
        HoltCredentialStore::load(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
