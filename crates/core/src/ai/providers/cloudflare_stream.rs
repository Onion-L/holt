//! Port of `pi-core/ai/src/providers/cloudflare-stream.ts`: the Cloudflare AI
//! Gateway URL placeholder wrapper. Providers built over Cloudflare route
//! their API stream through this shim so account/gateway placeholders
//! materialize from the resolved provider env before dispatch.

use std::sync::Arc;

use crate::ai::models::ProviderStreams;
use crate::ai::types::{Context, Model, ProviderEnv, SimpleStreamOptions, StreamOptions};
use crate::ai::utils::event_stream::AssistantMessageEventStream;

const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

/// Port of `resolveCloudflareModel`. Missing env entries keep their
/// placeholders; a model whose base URL is unchanged is returned as-is.
pub fn resolve_cloudflare_model(model: Model, env: Option<&ProviderEnv>) -> Model {
    let Some(env) = env else { return model };
    let account_id = env
        .get(CLOUDFLARE_ACCOUNT_ID)
        .cloned()
        .unwrap_or_else(|| format!("{{{CLOUDFLARE_ACCOUNT_ID}}}"));
    let gateway_id = env
        .get(CLOUDFLARE_GATEWAY_ID)
        .cloned()
        .unwrap_or_else(|| format!("{{{CLOUDFLARE_GATEWAY_ID}}}"));
    let base_url = model
        .base_url
        .replace(&format!("{{{CLOUDFLARE_ACCOUNT_ID}}}"), &account_id)
        .replace(&format!("{{{CLOUDFLARE_GATEWAY_ID}}}"), &gateway_id);
    if base_url == model.base_url {
        model
    } else {
        Model { base_url, ..model }
    }
}

/// Port of `cloudflareStreams`. Wraps the supplied adapter so every
/// stream/stream_simple call materializes the endpoint placeholders first.
pub fn cloudflare_streams(streams: Arc<dyn ProviderStreams>) -> Arc<dyn ProviderStreams> {
    struct CloudflareStreams(Arc<dyn ProviderStreams>);

    impl ProviderStreams for CloudflareStreams {
        fn stream(
            &self,
            model: &Model,
            context: &Context,
            options: Option<&StreamOptions>,
        ) -> AssistantMessageEventStream {
            let env = options.and_then(|options| options.base.env.as_ref());
            self.0.stream(
                &resolve_cloudflare_model(model.clone(), env),
                context,
                options,
            )
        }
        fn stream_simple(
            &self,
            model: &Model,
            context: &Context,
            options: Option<&SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            let env = options.and_then(|options| options.base.base.env.as_ref());
            self.0.stream_simple(
                &resolve_cloudflare_model(model.clone(), env),
                context,
                options,
            )
        }
    }
    Arc::new(CloudflareStreams(streams))
}
