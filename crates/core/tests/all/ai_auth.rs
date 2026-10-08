//! Tests for the auth port: credential store semantics, env-key auth
//! resolution, and provider auth resolution precedence.

use pi_core::ai::auth::context::default_provider_auth_context;
use pi_core::ai::auth::credential_store::InMemoryCredentialStore;
use pi_core::ai::auth::helpers::env_api_key_auth;
use pi_core::ai::auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, resolve_provider_auth,
};
use pi_core::ai::auth::types::{
    ApiKeyCredential, AuthResult, Credential, CredentialStore, OAuthCredential, ProviderAuth,
};

#[tokio::test]
async fn in_memory_credential_store_reads_writes_and_deletes() {
    let store = InMemoryCredentialStore::new();

    assert_eq!(store.read("anthropic", None).await.unwrap(), None);

    let credential = Credential::ApiKey(ApiKeyCredential {
        key: Some("sk-1".to_string()),
        env: None,
    });
    let credential_for_store = credential.clone();
    store
        .modify(
            "anthropic",
            Box::new(move |_| {
                Box::pin(async move { Ok(Some(credential_for_store)) })
                    as pi_core::ai::auth::types::AuthFuture<
                        Result<Option<Credential>, pi_core::ai::auth::types::BoxedAuthError>,
                    >
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        store.read("anthropic", None).await.unwrap(),
        Some(credential)
    );

    let listed = store.list(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider_id, "anthropic");
    assert_eq!(listed[0].credential_type, "api_key");

    store.delete("anthropic", None).await.unwrap();
    assert_eq!(store.read("anthropic", None).await.unwrap(), None);
}

#[tokio::test]
async fn credential_store_modify_propagates_caller_errors() {
    let store = InMemoryCredentialStore::new();
    let result = store
        .modify(
            "anthropic",
            Box::new(|_| {
                Box::pin(async {
                    Err::<Option<Credential>, _>(
                        ModelsError::new(ModelsErrorCode::OAuth, "refresh exploded").into_boxed(),
                    )
                })
                    as pi_core::ai::auth::types::AuthFuture<
                        Result<Option<Credential>, pi_core::ai::auth::types::BoxedAuthError>,
                    >
            }),
            None,
        )
        .await;

    let error = result.unwrap_err();
    let models_error = error.downcast_ref::<ModelsError>().expect("models error");
    assert_eq!(models_error.code, ModelsErrorCode::OAuth);
    // The store state is unchanged after a failed modify.
    assert_eq!(store.read("anthropic", None).await.unwrap(), None);
}

fn oauth_credential(expires_in_ms: i64) -> Credential {
    Credential::OAuth(OAuthCredential {
        refresh: "refresh-token".to_string(),
        access: "access-token".to_string(),
        expires: pi_core::ai::auth::resolve::now_millis() + expires_in_ms,
        extra: Default::default(),
    })
}

#[tokio::test]
async fn resolve_provider_auth_prefers_explicit_api_key_overrides() {
    let provider_auth = ProviderAuth::api_key(env_api_key_auth(
        "Anthropic API key",
        &["ANTHROPIC_API_KEY"],
    ));
    let credentials = InMemoryCredentialStore::new();
    let auth_context = default_provider_auth_context();

    let resolution = resolve_provider_auth(
        "anthropic",
        &provider_auth,
        &credentials,
        auth_context,
        Some(&AuthResolutionOverrides {
            api_key: Some("explicit-key".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let AuthResult { auth, source, .. } = resolution.expect("resolved");
    assert_eq!(auth.api_key.as_deref(), Some("explicit-key"));
    // envApiKeyAuth reports "stored credential" for any credential carrying a
    // key, including explicit overrides (matching the TS helper).
    assert_eq!(source.as_deref(), Some("stored credential"));
}

#[tokio::test]
async fn resolve_provider_auth_uses_stored_credential_before_env() {
    let provider_auth = ProviderAuth::api_key(env_api_key_auth(
        "Anthropic API key",
        &["ANTHROPIC_API_KEY"],
    ));
    let credentials = InMemoryCredentialStore::new();
    credentials
        .modify(
            "anthropic",
            Box::new(|_| {
                Box::pin(async move {
                    Ok(Some(Credential::ApiKey(ApiKeyCredential {
                        key: Some("stored-key".to_string()),
                        env: None,
                    })))
                })
                    as pi_core::ai::auth::types::AuthFuture<
                        Result<Option<Credential>, pi_core::ai::auth::types::BoxedAuthError>,
                    >
            }),
            None,
        )
        .await
        .unwrap();
    let auth_context = default_provider_auth_context();

    let resolution = resolve_provider_auth(
        "anthropic",
        &provider_auth,
        &credentials,
        auth_context,
        None,
    )
    .await
    .unwrap()
    .expect("resolved");
    assert_eq!(resolution.auth.api_key.as_deref(), Some("stored-key"));
    assert_eq!(resolution.source.as_deref(), Some("stored credential"));
}

#[tokio::test]
async fn resolve_provider_auth_returns_none_when_unconfigured() {
    let provider_auth = ProviderAuth::api_key(env_api_key_auth(
        "Anthropic API key",
        &["ANTHROPIC_API_KEY_unset_for_test"],
    ));
    let credentials = InMemoryCredentialStore::new();
    let auth_context = default_provider_auth_context();

    let resolution = resolve_provider_auth(
        "anthropic",
        &provider_auth,
        &credentials,
        auth_context,
        None,
    )
    .await
    .unwrap();
    assert_eq!(resolution, None);
}

#[tokio::test]
async fn resolve_provider_auth_fails_for_oauth_without_handler() {
    // A stored OAuth credential with a provider that only supports api keys
    // yields None (no silent env fallback).
    let provider_auth = ProviderAuth::api_key(env_api_key_auth(
        "Anthropic API key",
        &["ANTHROPIC_API_KEY_unset_for_test"],
    ));
    let credentials = InMemoryCredentialStore::new();
    credentials
        .modify(
            "anthropic",
            Box::new(|_| {
                Box::pin(async move { Ok(Some(oauth_credential(60 * 60 * 1000))) })
                    as pi_core::ai::auth::types::AuthFuture<
                        Result<Option<Credential>, pi_core::ai::auth::types::BoxedAuthError>,
                    >
            }),
            None,
        )
        .await
        .unwrap();
    let auth_context = default_provider_auth_context();

    let resolution = resolve_provider_auth(
        "anthropic",
        &provider_auth,
        &credentials,
        auth_context,
        None,
    )
    .await
    .unwrap();
    assert_eq!(resolution, None);
}

#[test]
fn models_error_appends_cause_detail_without_duplicating() {
    let error = ModelsError::with_cause(
        ModelsErrorCode::Auth,
        "Credential store read failed",
        &"disk on fire",
    );
    assert_eq!(error.message, "Credential store read failed: disk on fire");

    let duplicate = ModelsError::with_cause(
        ModelsErrorCode::Auth,
        "Credential store read failed: disk on fire",
        &"disk on fire",
    );
    assert_eq!(
        duplicate.message,
        "Credential store read failed: disk on fire"
    );

    let plain = ModelsError::new(ModelsErrorCode::Provider, "Unknown provider: x");
    assert_eq!(plain.code.as_str(), "provider");
}
