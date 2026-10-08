//! Port of `pi-core/ai/src/providers/cloudflare-ai-gateway.ts`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ai::auth::types::ProviderAuth;
use crate::ai::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};
use crate::ai::providers::apis::{
    anthropic_messages_api, openai_completions_api, openai_responses_api,
};
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::providers::cloudflare_auth::cloudflare_ai_gateway_auth;
use crate::ai::providers::cloudflare_stream::cloudflare_streams;

/// Port of `cloudflareAIGatewayProvider`.
pub fn cloudflare_ai_gateway_provider() -> Arc<dyn Provider> {
    let mut by_api: BTreeMap<String, Arc<dyn crate::ai::models::ProviderStreams>> = BTreeMap::new();
    by_api.insert(
        "anthropic-messages".to_string(),
        cloudflare_streams(anthropic_messages_api()),
    );
    by_api.insert(
        "openai-completions".to_string(),
        cloudflare_streams(openai_completions_api()),
    );
    by_api.insert(
        "openai-responses".to_string(),
        cloudflare_streams(openai_responses_api()),
    );
    create_provider(CreateProviderOptions {
        id: "cloudflare-ai-gateway".to_string(),
        organization_id: organization_for_provider("cloudflare-ai-gateway").map(str::to_string),
        name: Some("Cloudflare AI Gateway".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(cloudflare_ai_gateway_auth()),
        models: crate::ai::models_generated::models_for_provider("cloudflare-ai-gateway"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::ByApi(by_api),
    })
}
