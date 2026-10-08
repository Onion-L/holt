//! Port of `pi-core/ai/src/providers/anthropic.ts`: the Anthropic provider
//! with its bespoke API-key auth. Stored keys and `ANTHROPIC_API_KEY` /
//! `ANTHROPIC_OAUTH_TOKEN` resolve as `apiKey`, while `ANTHROPIC_AUTH_TOKEN`
//! resolves as an `Authorization: Bearer` header without the OAuth request
//! shaping.

use std::sync::Arc;

use crate::ai::auth::oauth::load::load_anthropic_oauth;
use crate::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthFuture, AuthInteraction, AuthPrompt,
    AuthPromptKind, AuthResult, AuthStorageError, ModelAuth, ProviderAuth,
};
use crate::ai::env_api_keys::{
    ANTHROPIC_API_KEY_ENV, ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_OAUTH_TOKEN_ENV,
};
use crate::ai::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};
use crate::ai::providers::apis::anthropic_messages_api;
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::types::ProviderHeaders;

/// Port of the `anthropicApiKeyAuth` value.
fn anthropic_api_key_auth() -> Arc<dyn ApiKeyAuth> {
    struct AnthropicApiKeyAuth;

    impl ApiKeyAuth for AnthropicApiKeyAuth {
        fn name(&self) -> &str {
            "Anthropic API key"
        }

        fn login(
            &self,
            interaction: Arc<dyn AuthInteraction>,
        ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
            Some(Box::pin(async move {
                if let Some(signal) = interaction.signal()
                    && signal.is_cancelled()
                {
                    return Err(AuthStorageError("The operation was aborted".to_string()));
                }
                let key = interaction
                    .prompt(AuthPrompt {
                        signal: interaction.signal(),
                        kind: AuthPromptKind::Secret {
                            message: "Enter Anthropic API key".to_string(),
                            placeholder: None,
                        },
                    })
                    .await?;
                if let Some(signal) = interaction.signal()
                    && signal.is_cancelled()
                {
                    return Err(AuthStorageError("The operation was aborted".to_string()));
                }
                Ok(ApiKeyCredential {
                    key: Some(key),
                    env: None,
                })
            }))
        }

        fn resolve(
            &self,
            input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
            Box::pin(async move {
                if input.signal.is_cancelled() {
                    return Err(AuthStorageError("The operation was aborted".to_string()));
                }
                if let Some(credential) = input.credential.as_ref()
                    && let Some(key) = credential.key.as_ref()
                    && !key.is_empty()
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key.clone()),
                            ..Default::default()
                        },
                        env: credential.env.clone(),
                        source: Some("stored credential".to_string()),
                    }));
                }

                let auth_token = input.ctx.env(ANTHROPIC_AUTH_TOKEN_ENV).await;
                if input.signal.is_cancelled() {
                    return Err(AuthStorageError("The operation was aborted".to_string()));
                }
                if let Some(auth_token) = auth_token.filter(|token| !token.is_empty()) {
                    let mut headers = ProviderHeaders::new();
                    headers.insert(
                        "Authorization".to_string(),
                        Some(format!("Bearer {auth_token}")),
                    );
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            headers: Some(headers),
                            ..Default::default()
                        },
                        env: None,
                        source: Some(ANTHROPIC_AUTH_TOKEN_ENV.to_string()),
                    }));
                }

                for env_var in [ANTHROPIC_OAUTH_TOKEN_ENV, ANTHROPIC_API_KEY_ENV] {
                    let api_key = input.ctx.env(env_var).await;
                    if input.signal.is_cancelled() {
                        return Err(AuthStorageError("The operation was aborted".to_string()));
                    }
                    if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
                        return Ok(Some(AuthResult {
                            auth: ModelAuth {
                                api_key: Some(api_key),
                                ..Default::default()
                            },
                            env: None,
                            source: Some(env_var.to_string()),
                        }));
                    }
                }
                Ok(None)
            })
        }
    }

    Arc::new(AnthropicApiKeyAuth)
}

/// Port of `anthropicProvider`.
pub fn anthropic_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "anthropic".to_string(),
        organization_id: organization_for_provider("anthropic").map(str::to_string),
        name: Some("Anthropic".to_string()),
        base_url: Some("https://api.anthropic.com".to_string()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(anthropic_api_key_auth()),
            oauth: Some(load_anthropic_oauth()),
        },
        models: crate::ai::models_generated::models_for_provider("anthropic"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(anthropic_messages_api()),
    })
}
