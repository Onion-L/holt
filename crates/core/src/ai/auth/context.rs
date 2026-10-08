//! Port of `pi-core/ai/src/auth/context.ts`: the default auth context backed
//! by the process environment and the filesystem (with `~` expansion).

use super::types::{AuthContext, AuthFuture};
use std::path::PathBuf;

/// Port of `defaultProviderAuthContext`.
pub fn default_provider_auth_context() -> std::sync::Arc<dyn AuthContext> {
    std::sync::Arc::new(DefaultProviderAuthContext)
}

struct DefaultProviderAuthContext;

impl AuthContext for DefaultProviderAuthContext {
    fn env(&self, name: &str) -> AuthFuture<Option<String>> {
        let name = name.to_string();
        Box::pin(async move {
            std::env::var(&name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
    }

    fn file_exists(&self, path: &str) -> AuthFuture<bool> {
        let path = path.to_string();
        Box::pin(async move { tokio::fs::metadata(expand_home(&path)).await.is_ok() })
    }
}

/// Expands a leading `~` to the user home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut expanded = PathBuf::from(home);
        if !rest.is_empty() {
            expanded.push(rest.trim_start_matches('/'));
        }
        return expanded;
    }
    PathBuf::from(path)
}
