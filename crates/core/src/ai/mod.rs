//! Port of `pi-core/ai/src/index.ts` (`@earendil-works/pi-ai` v0.84.4).
//!
//! Per-module porting status lives in `MIGRATION.md` at the repository root.

pub mod api;
pub mod auth;
pub mod cli;
pub mod compat;
pub mod env_api_keys;
pub mod images;
pub mod images_models;
pub mod model_catalog;
pub mod models;
pub mod models_generated;
pub mod models_store;
pub mod providers;
pub mod session_resources;
pub mod types;
pub mod utils;

/// Serializes `#[cfg(test)]` code that reads or mutates process
/// environment variables. Env-var access is process-global, so parallel
/// tests observing the ambient environment (for example
/// `env_api_keys::tests` and the bedrock credential tests) must share this
/// lock to avoid racing each other.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
