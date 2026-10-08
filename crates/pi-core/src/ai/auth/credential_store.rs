//! Port of `pi-core/ai/src/auth/credential-store.ts`: the in-memory default
//! credential store with per-provider serialized writes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::types::{
    AuthFuture, AuthOperationOptions, AuthStorageError, Credential, CredentialInfo, CredentialStore,
};

struct Inner {
    credentials: Mutex<HashMap<String, Credential>>,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl Inner {
    fn lock_for(&self, provider_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(provider_id.to_string()).or_default())
    }

    fn read_sync(&self, provider_id: &str) -> Option<Credential> {
        self.credentials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider_id)
            .cloned()
    }

    fn write_sync(&self, provider_id: &str, credential: Option<Credential>) {
        let mut map = self
            .credentials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match credential {
            Some(credential) => {
                map.insert(provider_id.to_string(), credential);
            }
            None => {
                map.remove(provider_id);
            }
        }
    }
}

/// Port of the `operationSignal(options?.signal)` + `throwIfAborted` pair: the
/// abort rejection the TypeScript store surfaces for cancelled operations.
fn abort_error() -> AuthStorageError {
    AuthStorageError("The operation was aborted".to_string())
}

fn operation_signal(options: Option<&AuthOperationOptions>) -> Option<CancellationToken> {
    options.and_then(|options| options.signal.clone())
}

/// Port of `InMemoryCredentialStore`. Writes are serialized per provider via
/// a per-provider async mutex, standing in for the TypeScript promise chain.
#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    inner: Arc<Inner>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            credentials: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>> {
        if operation_signal(options).is_some_and(|signal| signal.is_cancelled()) {
            return Box::pin(std::future::ready(Err(abort_error())));
        }
        let inner = Arc::clone(&self.inner);
        let provider_id = provider_id.to_string();
        Box::pin(async move { Ok(inner.read_sync(&provider_id)) })
    }

    fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>> {
        if operation_signal(options).is_some_and(|signal| signal.is_cancelled()) {
            return Box::pin(std::future::ready(Err(abort_error())));
        }
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let map = inner
                .credentials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(map
                .iter()
                .map(|(provider_id, credential)| CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: credential.type_name().to_string(),
                })
                .collect())
        })
    }

    fn modify(
        &self,
        provider_id: &str,
        modify: super::types::ModifyFn,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, super::types::BoxedAuthError>> {
        let inner = Arc::clone(&self.inner);
        let lock = inner.lock_for(provider_id);
        let provider_id = provider_id.to_string();
        let signal = operation_signal(options);
        Box::pin(async move {
            // Port of `raceWithAbortSignal` around the queued task: an aborted
            // signal rejects without waiting for the per-provider chain.
            let signal_cancelled = || signal.as_ref().is_some_and(|signal| signal.is_cancelled());
            if signal_cancelled() {
                return Err(abort_error().into());
            }
            let _guard = match &signal {
                Some(signal) => tokio::select! {
                    () = signal.cancelled() => return Err(abort_error().into()),
                    guard = lock.lock() => guard,
                },
                None => lock.lock().await,
            };
            // `enqueue` checks the signal once the chain settles, before the
            // task runs; a queued mutation is never executed after abort.
            if signal_cancelled() {
                return Err(abort_error().into());
            }
            let current = inner.read_sync(&provider_id);
            let next = modify(current.clone()).await?;
            // A mutation completing after its signal aborted is discarded.
            if signal_cancelled() {
                return Err(abort_error().into());
            }
            if let Some(credential) = &next {
                inner.write_sync(&provider_id, Some(credential.clone()));
            }
            // `next ?? current`: declining leaves the entry unchanged.
            Ok(next.or(current))
        })
    }

    fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>> {
        let inner = Arc::clone(&self.inner);
        let lock = inner.lock_for(provider_id);
        let provider_id = provider_id.to_string();
        let signal = operation_signal(options);
        Box::pin(async move {
            let signal_cancelled = || signal.as_ref().is_some_and(|signal| signal.is_cancelled());
            if signal_cancelled() {
                return Err(abort_error());
            }
            let _guard = match &signal {
                Some(signal) => tokio::select! {
                    () = signal.cancelled() => return Err(abort_error()),
                    guard = lock.lock() => guard,
                },
                None => lock.lock().await,
            };
            if signal_cancelled() {
                return Err(abort_error());
            }
            inner.write_sync(&provider_id, None);
            Ok(())
        })
    }
}
