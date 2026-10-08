//! Port of `pi-core/ai/src/providers/cloudflare-workers-ai.ts`.

use std::sync::Arc;

use crate::ai::auth::types::ProviderAuth;
use crate::ai::models::{CreateProviderOptions, Provider, ProviderApi, create_provider};
use crate::ai::providers::apis::openai_completions_api;
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::providers::cloudflare_auth::cloudflare_workers_ai_auth;
use crate::ai::providers::cloudflare_stream::cloudflare_streams;

/// Port of `cloudflareWorkersAIProvider`.
pub fn cloudflare_workers_ai_provider() -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: "cloudflare-workers-ai".to_string(),
        organization_id: organization_for_provider("cloudflare-workers-ai").map(str::to_string),
        name: Some("Cloudflare Workers AI".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth::api_key(cloudflare_workers_ai_auth()),
        models: crate::ai::models_generated::models_for_provider("cloudflare-workers-ai"),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(cloudflare_streams(openai_completions_api())),
    })
}
