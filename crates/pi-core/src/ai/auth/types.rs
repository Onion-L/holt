//! Port of `pi-core/ai/src/auth/types.ts`: the authentication contracts.
//!
//! Async callbacks become object-safe traits returning boxed futures; the
//! cancellation `AbortSignal` becomes `tokio_util::sync::CancellationToken`.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ai::types::ProviderEnv;
use tokio_util::sync::CancellationToken;

/// Boxed future used across the auth traits.
pub type AuthFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// Port of `ModelAuth`: request auth for a single model request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub base_url: Option<String>,
}

/// Port of `ApiKeyCredential`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// Port of `OAuthCredentials`: OAuth token data, with the open index
/// signature preserved as an extension map.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// JSON map alias for the `OAuthCredentials` extension fields.
type Map = serde_json::Map<String, serde_json::Value>;

/// Port of `OAuthCredential`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    #[serde(default)]
    pub refresh: String,
    #[serde(default)]
    pub access: String,
    #[serde(default)]
    pub expires: i64,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl From<OAuthCredentials> for OAuthCredential {
    fn from(credentials: OAuthCredentials) -> Self {
        Self {
            refresh: credentials.refresh,
            access: credentials.access,
            expires: credentials.expires,
            extra: credentials.extra,
        }
    }
}

impl From<OAuthCredential> for OAuthCredentials {
    fn from(credential: OAuthCredential) -> Self {
        Self {
            refresh: credential.refresh,
            access: credential.access,
            expires: credential.expires,
            extra: credential.extra,
        }
    }
}

/// Port of `Credential`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    /// The credential type discriminator.
    pub fn type_name(&self) -> &'static str {
        match self {
            Credential::ApiKey(_) => "api_key",
            Credential::OAuth(_) => "oauth",
        }
    }
}

/// Port of `CredentialInfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialInfo {
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "type")]
    pub credential_type: String,
}

/// Port of `AuthOperationOptions`.
#[derive(Clone, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<CancellationToken>,
}

/// Boxed error surfaced by credential store writes, carrying either a
/// storage failure or a caller-supplied error (e.g. OAuth refresh failures).
pub type BoxedAuthError = Box<dyn std::error::Error + Send + Sync>;

/// The serialized read-modify-write closure passed to
/// [`CredentialStore::modify`].
pub type ModifyFn = Box<
    dyn FnOnce(Option<Credential>) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>> + Send,
>;

/// Storage failures surfaced by credential stores.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthStorageError(pub String);

impl std::fmt::Display for AuthStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AuthStorageError {}

/// Port of `CredentialStore`: app-owned credential storage keyed by provider
/// id, one credential per provider. `modify` is the only write path, so every
/// mutation is a serialized read-modify-write.
pub trait CredentialStore: Send + Sync {
    /// Reads the stored credential, possibly expired; `Ok(None)` when absent.
    fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, AuthStorageError>>;

    /// Lists stored credential metadata without exposing secrets.
    /// Implementations must not execute configured API-key commands while
    /// listing.
    fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Vec<CredentialInfo>, AuthStorageError>>;

    /// Serialized write: `fn` sees the current credential and returns the new
    /// one, or `None` to leave the entry unchanged. Mutual exclusion is per
    /// provider id. Rejections from `fn` propagate.
    fn modify(
        &self,
        provider_id: &str,
        modify: ModifyFn,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<Option<Credential>, BoxedAuthError>>;

    /// Removes a credential (logout); serialized against `modify`.
    fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> AuthFuture<Result<(), AuthStorageError>>;
}

/// Environment and filesystem access for auth resolution.
pub trait AuthContext: Send + Sync {
    /// Reads an environment variable; `None` when unset or blank.
    fn env(&self, name: &str) -> AuthFuture<Option<String>>;
    /// Checks whether a file exists (supports a leading `~`).
    fn file_exists(&self, path: &str) -> AuthFuture<bool>;
}

/// Port of `AuthResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

/// Port of `AuthCheck`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub auth_type: AuthType,
}

/// Port of `AuthType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    #[default]
    ApiKey,
    OAuth,
}

/// Port of the `AuthPrompt` payload variants.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthPromptKind {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthPromptOption>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

/// A `select` prompt option.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthPromptOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// Port of `AuthPrompt` with its optional per-prompt cancellation signal.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthPrompt {
    pub signal: Option<CancellationToken>,
    pub kind: AuthPromptKind,
}

/// Port of `AuthInfoLink`.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

/// Port of `AuthEvent`.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// Port of `AuthInteraction`: login interaction callbacks.
pub trait AuthInteraction: Send + Sync {
    /// The whole-flow cancellation signal.
    fn signal(&self) -> Option<CancellationToken>;

    /// Prompts the user; returns the entered/selected string. Errors on
    /// cancel/abort.
    fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>>;

    /// Emits a login event.
    fn notify(&self, event: AuthEvent);
}

/// Inputs shared by api-key auth methods.
pub struct ApiKeyAuthInput {
    pub ctx: Arc<dyn AuthContext>,
    pub credential: Option<ApiKeyCredential>,
    pub signal: CancellationToken,
}

/// Port of `ApiKeyAuth`.
pub trait ApiKeyAuth: Send + Sync {
    /// Display name, e.g. "Anthropic API key".
    fn name(&self) -> &str;

    /// Interactive setup; absent means ambient-only.
    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
    ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
        None
    }

    /// Optional side-effect-free availability check.
    fn check(
        &self,
        _input: ApiKeyAuthInput,
    ) -> Option<AuthFuture<Result<Option<AuthCheck>, AuthStorageError>>> {
        None
    }

    /// Resolves auth from the stored credential and/or ambient sources.
    /// `None` = not configured.
    fn resolve(
        &self,
        input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>>;
}

/// Port of `OAuthAuth`.
pub trait OAuthAuth: Send + Sync {
    /// Display name, e.g. "Anthropic (Claude Pro/Max)".
    fn name(&self) -> &str;

    /// Whether access is backed by a provider subscription.
    fn is_subscription(&self) -> bool {
        false
    }

    /// Selector label for the OAuth login option.
    fn login_label(&self) -> Option<&str> {
        None
    }

    /// Runs the login flow.
    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>>;

    /// Exchanges the refresh token; a network call that errors on failure.
    fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: CancellationToken,
    ) -> AuthFuture<Result<OAuthCredential, AuthStorageError>>;

    /// Side-effect-free derivation of request auth from a valid credential.
    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> AuthFuture<Result<ModelAuth, AuthStorageError>>;
}

/// Port of `ProviderAuth`. At least one of `api_key`/`oauth` is present for
/// every provider.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

impl ProviderAuth {
    pub fn api_key(auth: Arc<dyn ApiKeyAuth>) -> Self {
        Self {
            api_key: Some(auth),
            oauth: None,
        }
    }

    pub fn oauth(auth: Arc<dyn OAuthAuth>) -> Self {
        Self {
            api_key: None,
            oauth: Some(auth),
        }
    }
}
