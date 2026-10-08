//! Port of `pi-core/ai/src/auth/helpers.ts`.

use super::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthFuture, AuthResult, AuthStorageError,
};
use std::sync::Arc;

/// Port of `envApiKeyAuth`: standard api-key auth. A stored credential key
/// wins; otherwise the first set env var resolves. Includes a `login` that
/// prompts for the key.
pub fn env_api_key_auth(name: impl Into<String>, env_vars: &[&str]) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars: env_vars.iter().map(|name| name.to_string()).collect(),
    })
}

struct EnvApiKeyAuth {
    name: String,
    env_vars: Vec<String>,
}

impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login(
        &self,
        interaction: Arc<dyn super::types::AuthInteraction>,
    ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
        let name = self.name.clone();
        Some(Box::pin(async move {
            if let Some(signal) = interaction.signal()
                && signal.is_cancelled()
            {
                return Err(AuthStorageError("The operation was aborted".to_string()));
            }
            let key = interaction
                .prompt(super::types::AuthPrompt {
                    signal: interaction.signal(),
                    kind: super::types::AuthPromptKind::Secret {
                        message: format!("Enter {name}"),
                        placeholder: None,
                    },
                })
                .await?;
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
        let env_vars = self.env_vars.clone();
        Box::pin(async move {
            if input.signal.is_cancelled() {
                return Err(AuthStorageError("The operation was aborted".to_string()));
            }
            if let Some(key) = input
                .credential
                .as_ref()
                .and_then(|credential| credential.key.clone())
            {
                return Ok(Some(AuthResult {
                    auth: super::types::ModelAuth {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    env: input
                        .credential
                        .as_ref()
                        .and_then(|credential| credential.env.clone()),
                    source: Some("stored credential".to_string()),
                }));
            }
            for env_var in env_vars {
                let value = input.ctx.env(&env_var).await;
                if input.signal.is_cancelled() {
                    return Err(AuthStorageError("The operation was aborted".to_string()));
                }
                if let Some(value) = value {
                    return Ok(Some(AuthResult {
                        auth: super::types::ModelAuth {
                            api_key: Some(value),
                            ..Default::default()
                        },
                        env: None,
                        source: Some(env_var),
                    }));
                }
            }
            Ok(None)
        })
    }
}
