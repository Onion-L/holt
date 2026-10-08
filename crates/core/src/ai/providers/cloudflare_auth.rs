//! Port of `pi-core/ai/src/providers/cloudflare-auth.ts`: the Cloudflare
//! API-key auth used by both the Workers AI and AI Gateway providers.
//!
//! Workers AI uses an `apiKey`; the AI Gateway instead ships the key as a
//! bearer in `cf-aig-authorization` and drops the SDK's `Authorization` /
//! `x-api-key` placeholders so the gateway does not treat them as a BYOK
//! provider key.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ai::auth::types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthFuture, AuthInteraction, AuthPrompt,
    AuthPromptKind, AuthResult, AuthStorageError, ModelAuth,
};
use crate::ai::types::{ProviderEnv, ProviderHeaders};

const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

#[derive(Clone, Copy)]
enum CloudflareKind {
    WorkersAi,
    AiGateway,
}

/// Port of `resolveValue`: per-field merge preferring the credential value,
/// falling back to ambient env. A credential carrying only the API key must
/// still pick up the account / gateway id from the environment.
async fn resolve_value(
    name: &str,
    input: &ApiKeyAuthInput,
) -> Result<Option<String>, AuthStorageError> {
    let from_credential = match input.credential.as_ref() {
        Some(credential) if name == CLOUDFLARE_API_KEY => credential.key.clone(),
        Some(credential) => credential
            .env
            .as_ref()
            .and_then(|env| env.get(name).cloned()),
        None => None,
    };
    if let Some(value) = from_credential {
        return Ok(Some(value));
    }
    if input.signal.is_cancelled() {
        return Err(AuthStorageError("The operation was aborted".to_string()));
    }
    let value = input.ctx.env(name).await;
    if input.signal.is_cancelled() {
        return Err(AuthStorageError("The operation was aborted".to_string()));
    }
    Ok(value)
}

/// JS truthiness: blank values count as unconfigured.
fn truthy(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

async fn resolve_env(
    kind: CloudflareKind,
    input: ApiKeyAuthInput,
) -> Result<Option<AuthResult>, AuthStorageError> {
    let api_key = resolve_value(CLOUDFLARE_API_KEY, &input).await?;
    let account_id = resolve_value(CLOUDFLARE_ACCOUNT_ID, &input).await?;
    let gateway_id = match kind {
        CloudflareKind::AiGateway => Some(resolve_value(CLOUDFLARE_GATEWAY_ID, &input).await?),
        CloudflareKind::WorkersAi => None,
    };

    let Some(api_key) = truthy(api_key) else {
        return Ok(None);
    };
    let Some(account_id) = truthy(account_id) else {
        return Ok(None);
    };
    let gateway_id = match gateway_id.and_then(truthy) {
        Some(value) => Some(value),
        None if matches!(kind, CloudflareKind::AiGateway) => return Ok(None),
        None => None,
    };

    let mut env = BTreeMap::new();
    env.insert(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id);
    if let Some(gateway_id) = gateway_id {
        env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
    }

    let auth = match kind {
        CloudflareKind::WorkersAi => ModelAuth {
            api_key: Some(api_key),
            ..Default::default()
        },
        CloudflareKind::AiGateway => {
            let mut headers = ProviderHeaders::new();
            headers.insert(
                "cf-aig-authorization".to_string(),
                Some(format!("Bearer {api_key}")),
            );
            headers.insert("Authorization".to_string(), None);
            headers.insert("x-api-key".to_string(), None);
            ModelAuth {
                headers: Some(headers),
                ..Default::default()
            }
        }
    };
    Ok(Some(AuthResult {
        auth,
        env: Some(env),
        source: Some(if input.credential.is_some() {
            "stored credential".to_string()
        } else {
            CLOUDFLARE_API_KEY.to_string()
        }),
    }))
}

struct CloudflareAuth {
    name: String,
    kind: CloudflareKind,
}

impl ApiKeyAuth for CloudflareAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> Option<AuthFuture<Result<ApiKeyCredential, AuthStorageError>>> {
        let kind = self.kind;
        Some(Box::pin(async move {
            if let Some(signal) = interaction.signal()
                && signal.is_cancelled()
            {
                return Err(AuthStorageError("The operation was aborted".to_string()));
            }
            let prompt = |kind: AuthPromptKind| AuthPrompt {
                signal: interaction.signal(),
                kind,
            };
            let key = interaction
                .prompt(prompt(AuthPromptKind::Secret {
                    message: "Enter Cloudflare API key".to_string(),
                    placeholder: None,
                }))
                .await?;
            let account_id = interaction
                .prompt(prompt(AuthPromptKind::Text {
                    message: "Enter Cloudflare account ID".to_string(),
                    placeholder: None,
                }))
                .await?;
            let mut env: ProviderEnv = BTreeMap::new();
            env.insert(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id);
            if matches!(kind, CloudflareKind::AiGateway) {
                let gateway_id = interaction
                    .prompt(prompt(AuthPromptKind::Text {
                        message: "Enter Cloudflare AI Gateway ID".to_string(),
                        placeholder: None,
                    }))
                    .await?;
                env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
            }
            Ok(ApiKeyCredential {
                key: Some(key),
                env: Some(env),
            })
        }))
    }

    fn resolve(
        &self,
        input: ApiKeyAuthInput,
    ) -> AuthFuture<Result<Option<AuthResult>, AuthStorageError>> {
        let kind = self.kind;
        Box::pin(async move { resolve_env(kind, input).await })
    }
}

/// Port of `cloudflareWorkersAIAuth`.
pub fn cloudflare_workers_ai_auth() -> Arc<dyn ApiKeyAuth> {
    Arc::new(CloudflareAuth {
        name: "Cloudflare API key".to_string(),
        kind: CloudflareKind::WorkersAi,
    })
}

/// Port of `cloudflareAIGatewayAuth`.
pub fn cloudflare_ai_gateway_auth() -> Arc<dyn ApiKeyAuth> {
    Arc::new(CloudflareAuth {
        name: "Cloudflare API key".to_string(),
        kind: CloudflareKind::AiGateway,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::auth::types::{AuthContext, AuthEvent, AuthPrompt};
    use std::collections::VecDeque;

    struct FixedEnvContext {
        env: ProviderEnv,
    }

    impl AuthContext for FixedEnvContext {
        fn env(&self, name: &str) -> AuthFuture<Option<String>> {
            Box::pin(std::future::ready(self.env.get(name).cloned()))
        }

        fn file_exists(&self, _path: &str) -> AuthFuture<bool> {
            Box::pin(std::future::ready(false))
        }
    }

    fn input(env: ProviderEnv, credential: Option<ApiKeyCredential>) -> ApiKeyAuthInput {
        ApiKeyAuthInput {
            ctx: Arc::new(FixedEnvContext { env }),
            credential,
            signal: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn env_from(pairs: &[(&str, &str)]) -> ProviderEnv {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn workers_ai_resolves_api_key_and_account_from_ambient_env() {
        let auth = cloudflare_workers_ai_auth();
        let resolved = auth
            .resolve(input(
                env_from(&[
                    ("CLOUDFLARE_API_KEY", "cf-key"),
                    ("CLOUDFLARE_ACCOUNT_ID", "account"),
                ]),
                None,
            ))
            .await
            .unwrap()
            .expect("configured");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("cf-key"));
        assert_eq!(resolved.auth.headers, None);
        assert_eq!(
            resolved.env,
            Some(env_from(&[("CLOUDFLARE_ACCOUNT_ID", "account")]))
        );
        assert_eq!(resolved.source.as_deref(), Some("CLOUDFLARE_API_KEY"));
    }

    #[tokio::test]
    async fn ai_gateway_merges_credential_and_env_per_field() {
        // Credential supplies the key and account; the gateway id must still
        // come from the ambient env (per-field merge).
        let auth = cloudflare_ai_gateway_auth();
        let resolved = auth
            .resolve(input(
                env_from(&[("CLOUDFLARE_GATEWAY_ID", "gateway")]),
                Some(ApiKeyCredential {
                    key: Some("cf-key".to_string()),
                    env: Some(env_from(&[("CLOUDFLARE_ACCOUNT_ID", "account")])),
                }),
            ))
            .await
            .unwrap()
            .expect("configured");
        assert_eq!(resolved.auth.api_key, None);
        let headers = resolved.auth.headers.expect("gateway headers");
        assert_eq!(
            headers
                .get("cf-aig-authorization")
                .and_then(|v| v.as_deref()),
            Some("Bearer cf-key")
        );
        assert_eq!(headers.get("Authorization"), Some(&None));
        assert_eq!(headers.get("x-api-key"), Some(&None));
        assert_eq!(
            resolved.env,
            Some(env_from(&[
                ("CLOUDFLARE_ACCOUNT_ID", "account"),
                ("CLOUDFLARE_GATEWAY_ID", "gateway"),
            ]))
        );
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));
    }

    #[tokio::test]
    async fn missing_required_values_resolve_to_none() {
        let workers = cloudflare_workers_ai_auth();
        // No account id: unconfigured.
        assert!(
            workers
                .resolve(input(env_from(&[("CLOUDFLARE_API_KEY", "cf-key")]), None))
                .await
                .unwrap()
                .is_none()
        );

        let gateway = cloudflare_ai_gateway_auth();
        // Gateway kind additionally requires the gateway id.
        assert!(
            gateway
                .resolve(input(
                    env_from(&[
                        ("CLOUDFLARE_API_KEY", "cf-key"),
                        ("CLOUDFLARE_ACCOUNT_ID", "account"),
                    ]),
                    None
                ))
                .await
                .unwrap()
                .is_none()
        );

        // The API key field only reads credential.key, never
        // credential.env[CLOUDFLARE_API_KEY] (TS resolveValue semantics).
        assert!(
            workers
                .resolve(input(
                    env_from(&[("CLOUDFLARE_ACCOUNT_ID", "account")]),
                    Some(ApiKeyCredential {
                        key: None,
                        env: Some(env_from(&[("CLOUDFLARE_API_KEY", "env-key")])),
                    }),
                ))
                .await
                .unwrap()
                .is_none()
        );
    }

    struct ScriptedInteraction {
        responses: std::sync::Mutex<VecDeque<String>>,
    }

    impl AuthInteraction for ScriptedInteraction {
        fn signal(&self) -> Option<tokio_util::sync::CancellationToken> {
            None
        }

        fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(AuthStorageError("no more responses".to_string()));
            Box::pin(std::future::ready(response))
        }

        fn notify(&self, _event: AuthEvent) {}
    }

    #[tokio::test]
    async fn gateway_login_prompts_key_account_and_gateway() {
        let auth = cloudflare_ai_gateway_auth();
        let interaction = Arc::new(ScriptedInteraction {
            responses: std::sync::Mutex::new(
                ["cf-key", "account", "gateway"]
                    .iter()
                    .map(|value| value.to_string())
                    .collect(),
            ),
        });
        let credential = auth
            .login(interaction)
            .expect("login supported")
            .await
            .unwrap();
        assert_eq!(credential.key.as_deref(), Some("cf-key"));
        assert_eq!(
            credential.env,
            Some(env_from(&[
                ("CLOUDFLARE_ACCOUNT_ID", "account"),
                ("CLOUDFLARE_GATEWAY_ID", "gateway"),
            ]))
        );
    }
}
