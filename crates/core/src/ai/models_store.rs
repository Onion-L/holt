//! Port of `pi-core/ai/src/models-store.ts`: persistent model catalogs keyed
//! by provider ID.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ai::types::Model;

/// Port of `ModelsStoreEntry`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    /// Unix timestamp from the remote catalog's Last-Modified header.
    pub last_modified: Option<i64>,
    /// Unix timestamp of the last completed remote check.
    pub checked_at: Option<i64>,
    /// Opaque validator from the remote catalog's ETag header, stored
    /// verbatim (quotes included) and echoed back as If-None-Match.
    pub etag: Option<String>,
}

/// Port of `ModelsStoreOperationOptions`.
#[derive(Clone, Default)]
pub struct ModelsStoreOperationOptions {
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// Storage failures surfaced by model stores.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelsStoreError(pub String);

impl std::fmt::Display for ModelsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ModelsStoreError {}

/// Port of `ModelsStore`.
pub trait ModelsStore: Send + Sync {
    fn read(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<Option<ModelsStoreEntry>, ModelsStoreError>>;

    fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<(), ModelsStoreError>>;

    fn delete(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<(), ModelsStoreError>>;
}

/// Port of `InMemoryModelsStore`.
#[derive(Clone, Default)]
pub struct InMemoryModelsStore {
    entries: Arc<Mutex<HashMap<String, ModelsStoreEntry>>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ModelsStore for InMemoryModelsStore {
    fn read(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<Option<ModelsStoreEntry>, ModelsStoreError>>
    {
        let entries = Arc::clone(&self.entries);
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            let map = entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Entries are stored cloned (structuredClone semantics).
            Ok(map.get(&provider_id).cloned())
        })
    }

    fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<(), ModelsStoreError>> {
        let entries = Arc::clone(&self.entries);
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(provider_id, entry);
            Ok(())
        })
    }

    fn delete(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> crate::ai::auth::types::AuthFuture<Result<(), ModelsStoreError>> {
        let entries = Arc::clone(&self.entries);
        let provider_id = provider_id.to_string();
        Box::pin(async move {
            entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&provider_id);
            Ok(())
        })
    }
}
