//! Port of `pi-core/ai/src/providers/amazon-bedrock.ts`: the Amazon Bedrock
//! provider with its credential-chain auth. Bedrock accepts a bearer token
//! or the AWS credential chain; the login flow stores a token/profile
//! choice, and resolve detects ambient AWS credentials without copying them
//! into pi's credential store.

use std::sync::Arc;

use crate::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthEvent, AuthFuture, AuthInfoLink,
    AuthInteraction, AuthPrompt, AuthPromptKind, AuthPromptOption, AuthResult, AuthStorageError,
    ModelAuth, ProviderAuth,
};
use crate::ai::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};
use crate::ai::providers::apis::bedrock_converse_stream_api;
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::types::ProviderEnv;

fn aborted() -> AuthStorageError {
    AuthStorageError("The operation was aborted".to_string())
}

async fn resolve_env_value(
    input: &ApiKeyAuthInput,
    name: &str,
) -> Result<Option<String>, AuthStorageError> {
    if input.signal.is_cancelled() {
        return Err(aborted());
    }
    let value = input.ctx.env(name).await;
    if input.signal.is_cancelled() {
        return Err(aborted());
    }
    Ok(value)
}

fn bedrock_auth() -> Arc<dyn ApiKeyAuth> {
    struct BedrockAuth;

    impl ApiKeyAuth for BedrockAuth {
        fn name(&self) -> &str {
            "AWS credentials or bearer token"
        }

        fn login(
            &self,
            interaction: Arc<dyn AuthInteraction>,
        ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
            Some(Box::pin(async move {
                if let Some(signal) = interaction.signal()
                    && signal.is_cancelled()
                {
                    return Err(aborted());
                }
                let method = interaction
                    .prompt(AuthPrompt {
                        signal: interaction.signal(),
                        kind: AuthPromptKind::Select {
                            message: "Select Amazon Bedrock authentication method:".to_string(),
                            options: vec![
                                AuthPromptOption {
                                    id: "bearer-token".to_string(),
                                    label: "Bearer token".to_string(),
                                    description: None,
                                },
                                AuthPromptOption {
                                    id: "aws-profile".to_string(),
                                    label: "AWS profile".to_string(),
                                    description: None,
                                },
                                AuthPromptOption {
                                    id: "credential-chain".to_string(),
                                    label: "Existing AWS credential chain".to_string(),
                                    description: None,
                                },
                            ],
                        },
                    })
                    .await?;
                if let Some(signal) = interaction.signal()
                    && signal.is_cancelled()
                {
                    return Err(aborted());
                }
                if method == "bearer-token" {
                    let key = interaction
                        .prompt(AuthPrompt {
                            signal: interaction.signal(),
                            kind: AuthPromptKind::Secret {
                                message: "Enter Amazon Bedrock bearer token".to_string(),
                                placeholder: None,
                            },
                        })
                        .await?;
                    return Ok(ApiKeyCredential {
                        key: Some(key),
                        env: None,
                    });
                }
                interaction.notify(AuthEvent::Info {
                    message: "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials.".to_string(),
                    links: vec![AuthInfoLink {
                        url: "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html".to_string(),
                        label: Some("AWS credential provider chain".to_string()),
                    }],
                });
                if method == "aws-profile" {
                    let profile = interaction
                        .prompt(AuthPrompt {
                            signal: interaction.signal(),
                            kind: AuthPromptKind::Text {
                                message: "Enter AWS profile name".to_string(),
                                placeholder: None,
                            },
                        })
                        .await?;
                    let mut env = ProviderEnv::new();
                    env.insert("AWS_PROFILE".to_string(), profile);
                    return Ok(ApiKeyCredential {
                        key: None,
                        env: Some(env),
                    });
                }
                if method != "credential-chain" {
                    return Err(AuthStorageError(format!(
                        "Unknown Amazon Bedrock auth method: {method}"
                    )));
                }
                interaction
                    .prompt(AuthPrompt {
                        signal: interaction.signal(),
                        kind: AuthPromptKind::Text {
                            message: "Configure AWS credentials, then press Enter to continue"
                                .to_string(),
                            placeholder: None,
                        },
                    })
                    .await?;
                Ok(ApiKeyCredential::default())
            }))
        }

        fn resolve(
            &self,
            input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
            Box::pin(async move {
                let credential = input.credential.as_ref();
                if let Some(key) = credential.and_then(|credential| credential.key.as_deref())
                    && !key.is_empty()
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key.to_string()),
                            ..Default::default()
                        },
                        env: credential.and_then(|credential| credential.env.clone()),
                        source: Some("stored credential".to_string()),
                    }));
                }
                if resolve_env_value(&input, "AWS_BEARER_TOKEN_BEDROCK")
                    .await?
                    .is_some_and(|token| !token.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some("AWS_BEARER_TOKEN_BEDROCK".to_string()),
                    }));
                }
                let credential_profile = credential
                    .and_then(|credential| credential.env.as_ref())
                    .and_then(|env| env.get("AWS_PROFILE").cloned());
                let profile = match credential_profile.clone() {
                    Some(profile) => Some(profile),
                    None => resolve_env_value(&input, "AWS_PROFILE").await?,
                };
                if profile.is_some_and(|profile| !profile.is_empty()) {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: credential.and_then(|credential| credential.env.clone()),
                        source: Some(
                            if credential_profile.is_some_and(|profile| !profile.is_empty()) {
                                "stored credential"
                            } else {
                                "AWS_PROFILE"
                            }
                            .to_string(),
                        ),
                    }));
                }
                let has_keys = resolve_env_value(&input, "AWS_ACCESS_KEY_ID")
                    .await?
                    .is_some_and(|key| !key.is_empty())
                    && resolve_env_value(&input, "AWS_SECRET_ACCESS_KEY")
                        .await?
                        .is_some_and(|key| !key.is_empty());
                if has_keys {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some("AWS access keys".to_string()),
                    }));
                }
                for (name, source) in [
                    ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ECS task role"),
                    ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ECS task role"),
                    ("AWS_WEB_IDENTITY_TOKEN_FILE", "web identity token"),
                ] {
                    if resolve_env_value(&input, name)
                        .await?
                        .is_some_and(|value| !value.is_empty())
                    {
                        return Ok(Some(AuthResult {
                            auth: ModelAuth::default(),
                            env: None,
                            source: Some(source.to_string()),
                        }));
                    }
                }
                Ok(None)
            })
        }
    }

    Arc::new(BedrockAuth)
}

/// Port of `amazonBedrockProvider`.
pub fn amazon_bedrock_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "amazon-bedrock".to_string(),
        organization_id: organization_for_provider("amazon-bedrock").map(str::to_string),
        name: Some("Amazon Bedrock".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(bedrock_auth()),
        models: crate::ai::models_generated::models_for_provider("amazon-bedrock"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(bedrock_converse_stream_api()),
    })
}
