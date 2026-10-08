//! Port of `pi-core/ai/src/auth/resolve.ts`: auth resolution shared by the
//! `Models` and images collections.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use super::types::{
    ApiKeyAuthInput, ApiKeyCredential, AuthContext, AuthOperationOptions, AuthResult,
    BoxedAuthError, Credential, CredentialStore, OAuthCredential, ProviderAuth,
};
use crate::ai::types::ProviderEnv;

/// Port of `ModelsErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelsErrorCode {
    ModelSource,
    ModelValidation,
    Provider,
    Stream,
    Auth,
    OAuth,
}

impl ModelsErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelsErrorCode::ModelSource => "model_source",
            ModelsErrorCode::ModelValidation => "model_validation",
            ModelsErrorCode::Provider => "provider",
            ModelsErrorCode::Stream => "stream",
            ModelsErrorCode::Auth => "auth",
            ModelsErrorCode::OAuth => "oauth",
        }
    }
}

/// Port of `ModelsError`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(code: ModelsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Port of the `withCauseDetail` constructor behavior: callers surface
    /// `error.message` only, so the underlying reason is appended when not
    /// already contained.
    pub fn with_cause(
        code: ModelsErrorCode,
        message: impl Into<String>,
        cause: &dyn std::fmt::Display,
    ) -> Self {
        let message = message.into();
        let detail = cause.to_string().trim().to_string();
        if detail.is_empty() || message.contains(&detail) {
            return Self { code, message };
        }
        Self {
            code,
            message: format!("{message}: {detail}"),
        }
    }

    /// Wraps the error for storage through `CredentialStore::modify`.
    pub fn into_boxed(self) -> BoxedAuthError {
        Box::new(self)
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ModelsError {}

/// Port of `AuthResolutionOverrides`.
#[derive(Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// Require this much remaining OAuth-token validity; defaults to five
    /// minutes.
    pub min_oauth_validity_ms: Option<u64>,
    pub signal: Option<CancellationToken>,
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Auth resolution errors: either a [`ModelsError`], a storage failure, or
/// cancellation.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolveError {
    Models(ModelsError),
    Storage(String),
    Aborted,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Models(error) => write!(f, "{error}"),
            ResolveError::Storage(message) => write!(f, "{message}"),
            ResolveError::Aborted => write!(f, "The operation was aborted"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<super::types::AuthStorageError> for ResolveError {
    fn from(error: super::types::AuthStorageError) -> Self {
        ResolveError::Storage(error.0)
    }
}

/// Port of `resolveProviderAuth`. A stored credential owns the provider:
/// ambient/env is consulted only when nothing is stored. No silent env
/// fallback after a failed refresh or for a credential type without a
/// matching handler.
pub async fn resolve_provider_auth(
    provider_id: &str,
    provider_auth: &ProviderAuth,
    credentials: &dyn CredentialStore,
    auth_context: Arc<dyn AuthContext>,
    overrides: Option<&AuthResolutionOverrides>,
) -> Result<Option<AuthResult>, ResolveError> {
    let signal = overrides
        .and_then(|overrides| overrides.signal.clone())
        .unwrap_or_default();
    if signal.is_cancelled() {
        return Err(ResolveError::Aborted);
    }
    // Port of the `raceWithAbortSignal` wrapper: stop waiting for a
    // non-cooperative resolution when the caller's signal aborts.
    let operation = resolve_provider_auth_with_signal(
        provider_id,
        provider_auth,
        credentials,
        auth_context,
        overrides,
        &signal,
    );
    tokio::select! {
        () = signal.cancelled() => Err(ResolveError::Aborted),
        result = operation => result,
    }
}

async fn resolve_provider_auth_with_signal(
    provider_id: &str,
    provider_auth: &ProviderAuth,
    credentials: &dyn CredentialStore,
    auth_context: Arc<dyn AuthContext>,
    overrides: Option<&AuthResolutionOverrides>,
    signal: &CancellationToken,
) -> Result<Option<AuthResult>, ResolveError> {
    let request_auth_context: Arc<dyn AuthContext> =
        match overrides.and_then(|overrides| overrides.env.as_ref()) {
            Some(env) => Arc::new(OverlayEnvAuthContext {
                base: Arc::clone(&auth_context),
                env: env.clone(),
            }),
            None => Arc::clone(&auth_context),
        };

    if let Some(api_key_override) = overrides.and_then(|overrides| overrides.api_key.clone())
        && let Some(api_key_auth) = provider_auth.api_key.as_ref()
    {
        let credential = ApiKeyCredential {
            key: Some(api_key_override),
            env: overrides.and_then(|overrides| overrides.env.clone()),
        };
        return resolve_api_key(
            &request_auth_context,
            api_key_auth.as_ref(),
            provider_id,
            Some(&credential),
            signal,
        )
        .await;
    }

    let stored = read_credential(credentials, provider_id, signal).await?;
    if let Some(stored) = stored {
        match (
            &stored,
            provider_auth.oauth.as_ref(),
            provider_auth.api_key.as_ref(),
        ) {
            (Credential::OAuth(oauth_credential), Some(oauth_auth), _) => {
                return resolve_stored_oauth(
                    credentials,
                    provider_id,
                    Arc::clone(oauth_auth),
                    oauth_credential.clone(),
                    signal,
                    overrides.and_then(|overrides| overrides.min_oauth_validity_ms),
                )
                .await;
            }
            (Credential::ApiKey(api_key_credential), _, Some(api_key_auth)) => {
                let credential =
                    if let Some(env) = overrides.and_then(|overrides| overrides.env.as_ref()) {
                        let mut merged = api_key_credential.clone();
                        let env_map: ProviderEnv = merged.env.clone().unwrap_or_default();
                        merged.env = Some(
                            env_map
                                .into_iter()
                                .chain(
                                    env.iter()
                                        .map(|(name, value)| (name.clone(), value.clone())),
                                )
                                .collect(),
                        );
                        merged
                    } else {
                        api_key_credential.clone()
                    };
                return resolve_api_key(
                    &request_auth_context,
                    api_key_auth.as_ref(),
                    provider_id,
                    Some(&credential),
                    signal,
                )
                .await;
            }
            // Credential type without a matching handler: not configured.
            _ => return Ok(None),
        }
    }

    // Ambient (env vars, AWS profiles, ADC files).
    match provider_auth.api_key.as_ref() {
        Some(api_key_auth) => {
            resolve_api_key(
                &request_auth_context,
                api_key_auth.as_ref(),
                provider_id,
                None,
                signal,
            )
            .await
        }
        None => Ok(None),
    }
}

struct OverlayEnvAuthContext {
    base: Arc<dyn AuthContext>,
    env: ProviderEnv,
}

impl AuthContext for OverlayEnvAuthContext {
    fn env(&self, name: &str) -> super::types::AuthFuture<Option<String>> {
        let value = self.env.get(name).cloned();
        let base = Arc::clone(&self.base);
        let name = name.to_string();
        Box::pin(async move {
            match value {
                Some(value) if !value.is_empty() => Some(value),
                _ => base.env(&name).await,
            }
        })
    }

    fn file_exists(&self, path: &str) -> super::types::AuthFuture<bool> {
        self.base.file_exists(path)
    }
}

const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;
const DEFAULT_OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

/// OAuth resolution with double-checked locking: tokens with less than five
/// minutes remaining lock, re-check expiry under the lock, refresh once
/// globally, and persist the rotated credential before release.
async fn resolve_stored_oauth(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    oauth: Arc<dyn super::types::OAuthAuth>,
    stored: OAuthCredential,
    signal: &CancellationToken,
    min_oauth_validity_ms: Option<u64>,
) -> Result<Option<AuthResult>, ResolveError> {
    let minimum_validity_ms =
        DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_oauth_validity_ms.unwrap_or(0));
    let expires_soon = move |credential: &OAuthCredential| {
        now_millis() + minimum_validity_ms as i64 >= credential.expires
    };
    let mut credential = stored;

    if expires_soon(&credential) {
        // Optimistic check said expired; the authoritative check runs under
        // the lock. The refresh runs under the caller signal plus a hard
        // timeout, mirroring AbortSignal.any([signal, timeout]).
        let refresh_token = CancellationToken::new();
        let watchers_done = CancellationToken::new();
        {
            let refresh_token = refresh_token.clone();
            let watchers_done = watchers_done.clone();
            let signal = signal.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = signal.cancelled() => refresh_token.cancel(),
                    () = watchers_done.cancelled() => {}
                }
            });
        }
        {
            let refresh_token = refresh_token.clone();
            let watchers_done = watchers_done.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_millis(
                        DEFAULT_OAUTH_REFRESH_TIMEOUT_MS,
                    )) => refresh_token.cancel(),
                    () = watchers_done.cancelled() => {}
                }
            });
        }

        let modify_oauth = Arc::clone(&oauth);
        let refresh_token_for_modify = refresh_token.clone();
        let provider_id_owned = provider_id.to_string();
        let expires_soon_for_modify = expires_soon;
        let modify: super::types::ModifyFn = Box::new(move |current| {
            Box::pin(async move {
                let Some(Credential::OAuth(current)) = current else {
                    // Logged out meanwhile.
                    return Ok(None);
                };
                if !expires_soon_for_modify(&current) {
                    // Another process/request refreshed.
                    return Ok(None);
                }
                match modify_oauth
                    .refresh(&current, refresh_token_for_modify)
                    .await
                {
                    Ok(refreshed) => Ok(Some(Credential::OAuth(refreshed))),
                    Err(error) => Err(ModelsError::with_cause(
                        ModelsErrorCode::OAuth,
                        format!("OAuth refresh failed for {provider_id_owned}"),
                        &error,
                    )
                    .into_boxed()),
                }
            }) as super::types::AuthFuture<Result<Option<Credential>, BoxedAuthError>>
        });
        let post = credentials
            .modify(
                provider_id,
                modify,
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await;

        // Release the watcher tasks.
        watchers_done.cancel();

        let post = post.map_err(|error| {
            if let Some(models_error) = error.downcast_ref::<ModelsError>() {
                ResolveError::Models(models_error.clone())
            } else {
                ResolveError::Models(ModelsError::with_cause(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    &error,
                ))
            }
        })?;
        let Some(Credential::OAuth(post)) = post else {
            // Logged out meanwhile.
            return Ok(None);
        };
        credential = post;
        // The normal five-minute window triggers a refresh but does not
        // impose a provider contract. Explicit callers do require the
        // requested minimum after the refresh.
        if min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(ResolveError::Models(ModelsError::new(
                ModelsErrorCode::OAuth,
                format!("OAuth refresh returned a token that expires too soon for {provider_id}"),
            )));
        }
    }

    match oauth.to_auth(&credential).await {
        Ok(auth) => Ok(Some(AuthResult {
            auth,
            env: None,
            source: Some("OAuth".to_string()),
        })),
        Err(error) => Err(ResolveError::Models(ModelsError::with_cause(
            ModelsErrorCode::OAuth,
            format!("OAuth auth derivation failed for {provider_id}"),
            &error,
        ))),
    }
}

async fn resolve_api_key(
    auth_context: &Arc<dyn AuthContext>,
    api_key_auth: &dyn super::types::ApiKeyAuth,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
    signal: &CancellationToken,
) -> Result<Option<AuthResult>, ResolveError> {
    match api_key_auth
        .resolve(ApiKeyAuthInput {
            ctx: Arc::clone(auth_context),
            credential: credential.cloned(),
            signal: signal.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Err(ResolveError::Models(ModelsError::with_cause(
            ModelsErrorCode::Auth,
            format!("API key auth failed for provider {provider_id}"),
            &error,
        ))),
    }
}

async fn read_credential(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    signal: &CancellationToken,
) -> Result<Option<Credential>, ResolveError> {
    if signal.is_cancelled() {
        return Err(ResolveError::Aborted);
    }
    match credentials
        .read(
            provider_id,
            Some(&AuthOperationOptions {
                signal: Some(signal.clone()),
            }),
        )
        .await
    {
        Ok(credential) => Ok(credential),
        Err(error) => Err(ResolveError::Models(ModelsError::with_cause(
            ModelsErrorCode::Auth,
            format!("Credential store read failed for {provider_id}"),
            &error,
        ))),
    }
}
