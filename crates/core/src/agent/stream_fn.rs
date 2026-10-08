//! Port of `pi-core/agent/src/stream-fn.ts`.

use std::sync::RwLock;

use super::types::StreamFn;

static DEFAULT_STREAM_FN: RwLock<Option<StreamFn>> = RwLock::new(None);

/// Port of `setDefaultStreamFn`: configure the fallback used by `Agent` and
/// the low-level loops when callers omit `stream_fn`.
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    *DEFAULT_STREAM_FN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = stream_fn;
}

/// Port of `getDefaultStreamFn`.
///
/// TypeScript throws when nothing is configured; the Rust port returns the
/// same message as an error.
pub fn get_default_stream_fn() -> Result<StreamFn, String> {
    DEFAULT_STREAM_FN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or_else(|| {
            "No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn()."
                .to_string()
        })
}
