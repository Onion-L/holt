//! Port of `pi-core/ai/src/api/openrouter-images.ts`: image generation over
//! the OpenRouter chat-completions endpoint with the `modalities` extension.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::ai::types::{
    AssistantImages, BlockContent, ImagesContext, ImagesModel, ImagesStopReason, ProviderHeaders,
    Usage,
};
use crate::ai::utils::error_body::{
    ProviderErrorParts, format_provider_error, normalize_provider_error,
};
use crate::ai::utils::http::{HttpBody, HttpRequest, collect_text};
use crate::ai::utils::provider_retry::{ProviderRetryOptions, retry_http_request};
use crate::ai::utils::reqwest_fetch::default_fetch;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;

pub use crate::ai::types::ImagesOptions;

/// Port of the `generateImages` images function.
pub async fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: Option<&ImagesOptions>,
) -> AssistantImages {
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: crate::ai::auth::resolve::now_millis(),
    };

    let result = run_generation(model, context, options, &mut output).await;
    if let Err(error) = result {
        let aborted = options
            .and_then(|options| options.signal.as_ref())
            .is_some_and(|token| token.is_cancelled());
        output.stop_reason = if aborted {
            ImagesStopReason::Aborted
        } else {
            ImagesStopReason::Error
        };
        output.error_message = Some(format_provider_error(
            &normalize_provider_error(ProviderErrorParts {
                status: None,
                body: None,
                message: error,
            }),
            None,
        ));
    }
    output
}

async fn run_generation(
    model: &ImagesModel,
    context: &ImagesContext,
    options: Option<&ImagesOptions>,
    output: &mut AssistantImages,
) -> Result<(), String> {
    let Some(api_key) = options.and_then(|options| options.api_key.clone()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };

    let mut params = build_params(model, context);
    if let Some(on_payload) = options.and_then(|options| options.on_payload.as_ref())
        && let Some(next_params) = on_payload(params.clone(), model).await
    {
        params = next_params;
    }

    let fetch = options
        .and_then(|options| options.fetch.clone())
        .unwrap_or_else(default_fetch);
    let request = HttpRequest {
        method: crate::ai::utils::http::HttpMethod::Post,
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers: build_headers(model, options, &api_key),
        body: HttpBody::Json(params),
        // The OpenAI SDK forwards the abort signal into its HTTP request; an
        // already-aborted signal rejects at the transport, like fetch.
        signal: options.and_then(|options| options.signal.clone()),
    };

    let response = retry_http_request(
        &fetch,
        &request,
        ProviderRetryOptions {
            max_retries: options.and_then(|options| options.max_retries),
            on_retry: options.and_then(|options| options.on_retry.clone()),
            max_retry_delay_ms: options.and_then(|options| options.max_retry_delay_ms),
            signal: options.and_then(|options| options.signal.clone()),
        },
    )
    .await
    .map_err(|error| error.message)?;

    // The OpenAI SDK surfaces non-2xx responses as thrown errors, so
    // `onResponse` only fires for successful responses.
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let body = collect_text(response).await;
        return Err(format_provider_error(
            &normalize_provider_error(ProviderErrorParts {
                status: Some(status),
                body: Some(body),
                message: format!("{status} status code"),
            }),
            None,
        ));
    }

    if let Some(on_response) = options.and_then(|options| options.on_response.as_ref()) {
        let response_headers: BTreeMap<String, String> = response
            .headers
            .iter()
            .map(|(name, value)| (name.to_lowercase(), value.clone()))
            .collect();
        on_response(
            &crate::ai::types::ProviderResponse {
                status: response.status,
                headers: response_headers,
            },
            model,
        )
        .await;
    }

    let body = collect_text(response).await;
    let image_response: Value = serde_json::from_str(&body)
        .map_err(|error| format!("invalid image response JSON: {error}"))?;

    output.response_id = image_response
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(usage) = image_response
        .get("usage")
        .filter(|usage| usage.is_object())
    {
        output.usage = Some(parse_usage(usage, model));
    }

    if let Some(choice) = image_response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        if let Some(content) = choice.pointer("/message/content").and_then(Value::as_str)
            && !content.is_empty()
        {
            output
                .output
                .push(BlockContent::Text(crate::ai::types::TextContent {
                    text: content.to_string(),
                    ..Default::default()
                }));
        }

        for image in choice
            .pointer("/message/images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let image_url = match image.get("image_url") {
                Some(Value::String(url)) => Some(url.clone()),
                Some(Value::Object(object)) => object
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            };
            let Some(image_url) = image_url.filter(|url| url.starts_with("data:")) else {
                continue;
            };
            // Port of /^data:([^;]+);base64,(.+)$/ parsing.
            let Some(rest) = image_url.strip_prefix("data:") else {
                continue;
            };
            let Some((mime_type, data)) = rest.split_once(";base64,") else {
                continue;
            };
            output
                .output
                .push(BlockContent::Image(crate::ai::types::ImageContent {
                    content_type: Default::default(),
                    mime_type: mime_type.to_string(),
                    data: data.to_string(),
                }));
        }
    }

    Ok(())
}

/// Port of `createClient` header assembly: `providerHeadersToRecord` over
/// `{...model.headers, ...options.headers}` plus the SDK bearer header.
/// A `None` options value suppresses the model header of the same name.
fn build_headers(
    model: &ImagesModel,
    options: Option<&ImagesOptions>,
    api_key: &str,
) -> Vec<(String, String)> {
    let mut merged: ProviderHeaders = BTreeMap::new();
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            merged.insert(name.clone(), Some(value.clone()));
        }
    }
    if let Some(options_headers) = options.and_then(|options| options.headers.as_ref()) {
        for (name, value) in options_headers {
            merged.insert(name.clone(), value.clone());
        }
    }

    let mut headers: Vec<(String, String)> =
        vec![("authorization".to_string(), format!("Bearer {api_key}"))];
    for (name, value) in merged {
        if let Some(value) = value {
            headers.push((name, value));
        }
    }
    headers
}

/// Port of `buildParams`.
pub fn build_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            BlockContent::Text(text) => json!({
                "type": "text",
                "text": sanitize_surrogates(&text.text),
            }),
            BlockContent::Image(image) => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
            }),
        })
        .collect();

    json!({
        "model": model.id,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "modalities": if model.output.contains(&crate::ai::types::ModelInput::Text) {
            json!(["image", "text"])
        } else {
            json!(["image"])
        },
    })
}

/// Port of `parseUsage` (image flavor): cache-write tokens are subtracted
/// from the reported cached count.
fn parse_usage(raw_usage: &Value, model: &ImagesModel) -> Usage {
    let get_u64 = |key: &str| raw_usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let prompt_tokens = get_u64("prompt_tokens");
    let details = raw_usage.get("prompt_tokens_details");
    let reported_cached_tokens = details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write_tokens = details
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_tokens = if cache_write_tokens > 0 {
        reported_cached_tokens.saturating_sub(cache_write_tokens)
    } else {
        reported_cached_tokens
    };
    let input = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    let output = get_u64("completion_tokens");

    let rates = &model.cost.rates;
    let input_cost = (rates.input.0 / 1_000_000.0) * input as f64;
    let output_cost = (rates.output.0 / 1_000_000.0) * output as f64;
    let cache_read_cost = (rates.cache_read.0 / 1_000_000.0) * cache_read_tokens as f64;
    let cache_write_cost = (rates.cache_write.0 / 1_000_000.0) * cache_write_tokens as f64;
    let total = input_cost + output_cost + cache_read_cost + cache_write_cost;

    Usage {
        input,
        output,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        total_tokens: input + output + cache_read_tokens + cache_write_tokens,
        cost: crate::ai::types::UsageCost {
            input: input_cost.into(),
            output: output_cost.into(),
            cache_read: cache_read_cost.into(),
            cache_write: cache_write_cost.into(),
            total: total.into(),
        },
        ..Default::default()
    }
}
