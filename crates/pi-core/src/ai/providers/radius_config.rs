//! Port of `pi-core/ai/src/providers/radius-config.ts`: Radius gateway URL
//! and credential-config helpers shared by the OAuth flow and the provider.

use serde_json::Value;

use crate::ai::auth::types::OAuthCredential;
use crate::ai::types::{Model, ModelCost};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

/// Port of `RadiusGatewayModel`.
#[derive(Clone, Debug, PartialEq)]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub thinking_level_map: Option<crate::ai::types::ThinkingLevelMap>,
    pub input: Vec<crate::ai::types::ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// Port of `RadiusGatewayConfig`.
#[derive(Clone, Debug, PartialEq)]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

/// Port of `normalizeRadiusGatewayUrl`.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let with_scheme = if regex::Regex::new(r"(?i)^https?://")
        .unwrap()
        .is_match(value)
    {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Port of `getRadiusCredentialConfig`: reads the `gatewayConfig` extension
/// from a stored credential, sanitized.
pub fn get_radius_credential_config(
    credential: Option<&OAuthCredential>,
) -> Option<RadiusGatewayConfig> {
    let config = credential?.extra.get("gatewayConfig")?;
    sanitize_radius_gateway_config(config)
}

/// Port of `getRadiusModelsFromConfig`.
pub fn get_radius_models_from_config(
    provider_id: &str,
    config: &RadiusGatewayConfig,
) -> Vec<Model> {
    config
        .models
        .iter()
        .map(|model| Model {
            id: model.id.clone(),
            name: model.name.clone(),
            api: "pi-messages".to_string(),
            provider: provider_id.to_string(),
            base_url: config.base_url.clone(),
            reasoning: model.reasoning,
            thinking_level_map: model.thinking_level_map.clone(),
            input: model.input.clone(),
            cost: model.cost.clone(),
            context_window: model.context_window,
            max_tokens: model.max_tokens,
            ..Default::default()
        })
        .collect()
}

/// Port of `getRadiusModels`.
pub fn get_radius_models(provider_id: &str, credential: Option<&OAuthCredential>) -> Vec<Model> {
    get_radius_credential_config(credential)
        .map(|config| get_radius_models_from_config(provider_id, &config))
        .unwrap_or_default()
}

fn sanitize_radius_gateway_config(config: &Value) -> Option<RadiusGatewayConfig> {
    let Value::Object(config) = config else {
        return None;
    };
    let Value::String(base_url) = config.get("baseUrl")? else {
        return None;
    };
    let Value::Array(models) = config.get("models")? else {
        return None;
    };
    Some(RadiusGatewayConfig {
        base_url: base_url.clone(),
        models: models.iter().filter_map(radius_gateway_model).collect(),
    })
}

fn radius_gateway_model(value: &Value) -> Option<RadiusGatewayModel> {
    let Value::Object(model) = value else {
        return None;
    };
    Some(RadiusGatewayModel {
        id: model.get("id")?.as_str()?.to_string(),
        name: model.get("name")?.as_str()?.to_string(),
        reasoning: model.get("reasoning")?.as_bool()?,
        thinking_level_map: model
            .get("thinkingLevelMap")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        input: model
            .get("input")?
            .as_array()?
            .iter()
            .map(|input| match input.as_str() {
                Some("text") => Some(crate::ai::types::ModelInput::Text),
                Some("image") => Some(crate::ai::types::ModelInput::Image),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        cost: serde_json::from_value(model.get("cost")?.clone()).ok()?,
        context_window: model.get("contextWindow")?.as_u64()?,
        max_tokens: model.get("maxTokens")?.as_u64()?,
    })
}

/// Port of the radius-local `truncateHttpBody`: trimmed and capped at 512
/// chars with an ellipsis.
fn truncate_http_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() > 512 {
        format!("{}\u{2026}", trimmed.chars().take(512).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

/// Port of `loadRadiusGatewayConfig`: fetches `{gateway}/v1/config`. The
/// TypeScript global `fetch` maps to an injectable transport defaulting to
/// the reqwest-backed [`crate::ai::utils::reqwest_fetch::default_fetch`];
/// like the other ported transports it cannot observe the signal, so a
/// pre-flight cancellation check stands in for signal forwarding.
pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    signal: Option<&tokio_util::sync::CancellationToken>,
    fetch: Option<std::sync::Arc<dyn crate::ai::utils::http::HttpFetch>>,
) -> Result<RadiusGatewayConfig, String> {
    if signal.is_some_and(|signal| signal.is_cancelled()) {
        return Err("The operation was aborted".to_string());
    }
    let mut headers = vec![("accept".to_string(), "application/json".to_string())];
    if let Some(api_key) = api_key {
        headers.push(("authorization".to_string(), format!("Bearer {api_key}")));
    }
    let fetch = fetch.unwrap_or_else(crate::ai::utils::reqwest_fetch::default_fetch);
    let response = fetch
        .fetch(crate::ai::utils::http::HttpRequest {
            signal: None,
            method: crate::ai::utils::http::HttpMethod::Get,
            url: format!("{gateway}/v1/config"),
            headers,
            body: crate::ai::utils::http::HttpBody::Empty,
        })
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status;
    let text = crate::ai::utils::http::collect_text(response).await;
    if !(200..300).contains(&status) {
        return Err(format!(
            "Could not load Radius config from {gateway}: {status}: {}",
            truncate_http_body(&text)
        ));
    }
    let config: Value = serde_json::from_str(&text)
        .map_err(|_| "Invalid Radius config from {gateway}".to_string())?;
    sanitize_radius_gateway_config(&config)
        .ok_or_else(|| format!("Invalid Radius config from {gateway}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_gateway_urls() {
        assert_eq!(
            normalize_radius_gateway_url("radius.pi.dev"),
            "https://radius.pi.dev"
        );
        assert_eq!(
            normalize_radius_gateway_url("https://radius.example/"),
            "https://radius.example"
        );
        assert_eq!(
            normalize_radius_gateway_url("http://localhost:8080//"),
            "http://localhost:8080"
        );
    }

    #[test]
    fn reads_sanitized_credential_configs() {
        let credential = OAuthCredential {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: 0,
            extra: [(
                "gatewayConfig".to_string(),
                serde_json::json!({
                    "baseUrl": "https://gw.example",
                    "models": [
                        {
                            "id": "m1",
                            "name": "M1",
                            "reasoning": true,
                            "input": ["text"],
                            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
                            "contextWindow": 1000,
                            "maxTokens": 100
                        },
                        {"id": "broken"}
                    ]
                }),
            )]
            .into_iter()
            .collect(),
        };
        let models = get_radius_models("radius", Some(&credential));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert_eq!(models[0].api, "pi-messages");
        assert_eq!(models[0].provider, "radius");
        assert_eq!(models[0].base_url, "https://gw.example");
        assert!(get_radius_models("radius", None).is_empty());
    }
}
