//! Port of `pi-core/ai/src/providers/google-vertex.ts`: the Google Vertex AI
//! provider. Vertex accepts an explicit API key or Application Default
//! Credentials (`gcloud auth application-default login`); ADC additionally
//! requires project and location, which the API implementation reads itself.

use std::sync::Arc;

use crate::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthEvent, AuthFuture, AuthInfoLink,
    AuthInteraction, AuthPrompt, AuthPromptKind, AuthPromptOption, AuthResult, AuthStorageError,
    ModelAuth, ProviderAuth,
};
use crate::ai::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};
use crate::ai::providers::apis::google_vertex_api;
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::types::ProviderEnv;

const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

/// Reads one env var with the TS cancellation points around the context
/// call.
async fn resolve_env_value(
    input: &ApiKeyAuthInput,
    name: &str,
) -> Result<Option<String>, AuthStorageError> {
    if input.signal.is_cancelled() {
        return Err(AuthStorageError("The operation was aborted".to_string()));
    }
    let value = input.ctx.env(name).await;
    if input.signal.is_cancelled() {
        return Err(AuthStorageError("The operation was aborted".to_string()));
    }
    Ok(value)
}

fn aborted() -> AuthStorageError {
    AuthStorageError("The operation was aborted".to_string())
}

/// Port of the `vertexAuth` value.
fn vertex_auth() -> Arc<dyn ApiKeyAuth> {
    struct VertexAuth;

    impl ApiKeyAuth for VertexAuth {
        fn name(&self) -> &str {
            "Google Cloud credentials"
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
                            message: "Select Google Vertex AI authentication method:".to_string(),
                            options: vec![
                                AuthPromptOption {
                                    id: "api-key".to_string(),
                                    label: "Google Cloud API key".to_string(),
                                    description: None,
                                },
                                AuthPromptOption {
                                    id: "adc".to_string(),
                                    label: "Application Default Credentials".to_string(),
                                    description: None,
                                },
                                AuthPromptOption {
                                    id: "service-account".to_string(),
                                    label: "Service account credentials file".to_string(),
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
                if method == "api-key" {
                    let key = interaction
                        .prompt(AuthPrompt {
                            signal: interaction.signal(),
                            kind: AuthPromptKind::Secret {
                                message: "Enter Google Cloud API key".to_string(),
                                placeholder: None,
                            },
                        })
                        .await?;
                    return Ok(ApiKeyCredential {
                        key: Some(key),
                        env: None,
                    });
                }
                if method != "adc" && method != "service-account" {
                    return Err(AuthStorageError(format!(
                        "Unknown Google Vertex AI auth method: {method}"
                    )));
                }
                interaction.notify(AuthEvent::Info {
                    message: if method == "adc" {
                        "Run `gcloud auth application-default login`, then provide the project and location.".to_string()
                    } else {
                        "Provide a service account credentials file, project, and location.".to_string()
                    },
                    links: vec![AuthInfoLink {
                        url: "https://cloud.google.com/docs/authentication/provide-credentials-adc"
                            .to_string(),
                        label: Some("Application Default Credentials".to_string()),
                    }],
                });
                let project = interaction
                    .prompt(AuthPrompt {
                        signal: interaction.signal(),
                        kind: AuthPromptKind::Text {
                            message: "Enter Google Cloud project ID".to_string(),
                            placeholder: None,
                        },
                    })
                    .await?;
                let location = interaction
                    .prompt(AuthPrompt {
                        signal: interaction.signal(),
                        kind: AuthPromptKind::Text {
                            message: "Enter Google Cloud location".to_string(),
                            placeholder: None,
                        },
                    })
                    .await?;
                let credentials_path = if method == "service-account" {
                    Some(
                        interaction
                            .prompt(AuthPrompt {
                                signal: interaction.signal(),
                                kind: AuthPromptKind::Text {
                                    message: "Enter service account credentials file path"
                                        .to_string(),
                                    placeholder: None,
                                },
                            })
                            .await?,
                    )
                } else {
                    None
                };
                let mut env = ProviderEnv::new();
                env.insert("GOOGLE_CLOUD_PROJECT".to_string(), project);
                env.insert("GOOGLE_CLOUD_LOCATION".to_string(), location);
                if let Some(credentials_path) = credentials_path {
                    env.insert(
                        "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                        credentials_path,
                    );
                }
                Ok(ApiKeyCredential {
                    key: None,
                    env: Some(env),
                })
            }))
        }

        fn resolve(
            &self,
            input: ApiKeyAuthInput,
        ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
            Box::pin(async move {
                // Nullish reads from the credential, env fallback per field.
                let credential_env = input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.env.clone());
                let env_field = |name: &str| {
                    credential_env
                        .as_ref()
                        .and_then(|env| env.get(name).cloned())
                };

                let key = match input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.key.clone())
                {
                    Some(key) => Some(key),
                    None => resolve_env_value(&input, "GOOGLE_CLOUD_API_KEY").await?,
                };
                if let Some(key) = key.filter(|key| !key.is_empty()) {
                    let from_credential = input
                        .credential
                        .as_ref()
                        .and_then(|credential| credential.key.as_deref())
                        .is_some_and(|key| !key.is_empty());
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key),
                            ..Default::default()
                        },
                        env: None,
                        source: Some(
                            if from_credential {
                                "stored credential"
                            } else {
                                "GOOGLE_CLOUD_API_KEY"
                            }
                            .to_string(),
                        ),
                    }));
                }

                let adc_path = match env_field("GOOGLE_APPLICATION_CREDENTIALS") {
                    Some(path) => Some(path),
                    None => resolve_env_value(&input, "GOOGLE_APPLICATION_CREDENTIALS").await?,
                };
                if input.signal.is_cancelled() {
                    return Err(aborted());
                }
                let has_credentials = input
                    .ctx
                    .file_exists(adc_path.as_deref().unwrap_or(VERTEX_ADC_PATH))
                    .await;
                if input.signal.is_cancelled() {
                    return Err(aborted());
                }
                let mut project = env_field("GOOGLE_CLOUD_PROJECT");
                if project.is_none() {
                    project = resolve_env_value(&input, "GOOGLE_CLOUD_PROJECT").await?;
                }
                if project.is_none() {
                    project = resolve_env_value(&input, "GCLOUD_PROJECT").await?;
                }
                let mut location = env_field("GOOGLE_CLOUD_LOCATION");
                if location.is_none() {
                    location = resolve_env_value(&input, "GOOGLE_CLOUD_LOCATION").await?;
                }
                if has_credentials
                    && project.is_some_and(|project| !project.is_empty())
                    && location.is_some_and(|location| !location.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: credential_env,
                        source: Some(
                            if input.credential.is_some() {
                                "stored credential"
                            } else {
                                "gcloud application default credentials"
                            }
                            .to_string(),
                        ),
                    }));
                }
                Ok(None)
            })
        }
    }

    Arc::new(VertexAuth)
}

/// Port of `googleVertexProvider`.
pub fn google_vertex_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "google-vertex".to_string(),
        organization_id: organization_for_provider("google-vertex").map(str::to_string),
        name: Some("Google Vertex AI".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(vertex_auth()),
        models: crate::ai::models_generated::models_for_provider("google-vertex"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(google_vertex_api()),
    })
}
