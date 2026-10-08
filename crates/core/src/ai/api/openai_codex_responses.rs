//! Port of `pi-core/ai/src/api/openai-codex-responses.ts`.
//!
//! The adapter prefers the WebSocket transport (`auto`, `websocket`,
//! `websocket-cached`) with SSE fallback; the WebSocket machinery lives in
//! [`super::openai_codex_websocket`] and the SSE path (request shape, URL
//! resolution, retry policy, zstd request-body compression, and the Codex
//! event mapping) is ported here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::ai::api::openai_codex_websocket::{
    AcquiredWebSocket, CodexWsFailure, WebSocketEvent, acquire_web_socket,
    is_websocket_sse_fallback_active, record_websocket_failure, record_websocket_sse_fallback,
};
use crate::ai::api::openai_completions::clamp_openai_prompt_cache_key;
use crate::ai::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ResponsesStreamOptions, convert_responses_messages,
    convert_responses_tools, process_responses_stream,
};
use crate::ai::api::simple_options::build_base_options;
use crate::ai::models::clamp_thinking_level;
use crate::ai::types::{
    CacheRetention, Context, Model, ProviderHeaders, SimpleStreamOptions, StreamOptions,
    ThinkingLevel, Transport, Usage,
};
use crate::ai::utils::deferred_tools::split_deferred_tools;
use crate::ai::utils::diagnostics::{
    append_assistant_message_diagnostic, create_assistant_message_diagnostic,
};
use crate::ai::utils::error_body::{
    ProviderErrorParts, format_provider_error, normalize_provider_error,
};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use crate::ai::utils::http::{HttpBody, HttpMethod, HttpRequest};
use crate::ai::utils::provider_retry::retry_provider_request;
use crate::ai::utils::reqwest_fetch::default_fetch;

pub use crate::ai::api::openai_codex_websocket::{
    OpenAICodexWebSocketDebugStats, close_openai_codex_websocket_sessions,
    get_openai_codex_websocket_debug_stats, reset_openai_codex_websocket_debug_stats,
    set_websocket_clock_for_tests, set_websocket_factory_for_tests,
};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
/// The Codex backend accepts zstd-compressed request bodies on the SSE
/// responses endpoint (the same endpoint the official Codex client
/// compresses against).
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;

fn codex_tool_call_providers() -> BTreeSet<String> {
    ["openai", "openai-codex", "opencode"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Port of `OpenAICodexResponsesOptions`.
#[derive(Clone, Default)]
pub struct OpenAICodexResponsesOptions {
    pub base: StreamOptions,
    /// "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max".
    pub reasoning_effort: Option<String>,
    /// "auto" | "concise" | "detailed" | "off" | "on".
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    /// "low" | "medium" | "high".
    pub text_verbosity: Option<String>,
    /// "auto" | "none" | "required".
    pub tool_choice: Option<String>,
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    let expected = name.to_lowercase();
    headers.iter().any(|(key, value)| {
        key.to_lowercase() == expected
            && value.as_ref().is_some_and(|value| !value.trim().is_empty())
    })
}

/// Port of `getApiKeyValue` equivalent: explicit key or header-owned auth.
fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, String> {
    if let Some(api_key) = api_key {
        return Ok(api_key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    Err(format!("No API key for provider: {provider}"))
}

/// Port of `extractAccountId`: decodes the JWT payload claim
/// `https://api.openai.com/auth.chatgpt_account_id`.
fn extract_account_id(auth_token: &str) -> Option<String> {
    let token = auth_token.strip_prefix("Bearer ").unwrap_or(auth_token);
    let mut parts = token.split('.');
    let (_header, payload) = (parts.next()?, parts.next()?);
    let decoded = decode_base64_url(payload)?;
    let parsed: Value = serde_json::from_str(&decoded).ok()?;
    // The claim key contains '/' characters, so a JSON pointer cannot address
    // it; read the object keys directly.
    parsed
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Minimal URL-safe base64 decoder (no external dependency).
fn decode_base64_url(input: &str) -> Option<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut values = Vec::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '-' => values.push(62),
            '_' => values.push(63),
            '=' => break,
            other => {
                let index = ALPHABET
                    .iter()
                    .position(|candidate| *candidate as char == other)?;
                values.push(index as u32);
            }
        }
    }
    let mut bytes = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        match chunk.len() {
            4 => {
                let n = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6) | chunk[3];
                bytes.push((n >> 16) as u8);
                bytes.push((n >> 8) as u8);
                bytes.push(n as u8);
            }
            3 => {
                let n = (chunk[0] << 18) | (chunk[1] << 12) | (chunk[2] << 6);
                bytes.push((n >> 16) as u8);
                bytes.push((n >> 8) as u8);
            }
            2 => {
                let n = (chunk[0] << 18) | (chunk[1] << 12);
                bytes.push((n >> 16) as u8);
            }
            _ => return None,
        }
    }
    String::from_utf8(bytes).ok()
}

/// Port of `resolveCodexUrl`.
pub fn resolve_codex_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .filter(|url| !url.trim().is_empty())
        .unwrap_or(DEFAULT_CODEX_BASE_URL);
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Port of `resolveCodexWebSocketUrl`: the responses URL with the scheme
/// swapped to `ws`/`wss` (URL serialization keeps the normalized path).
pub fn resolve_codex_websocket_url(base_url: Option<&str>) -> String {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url
    }
}

/// Headers-object semantics for the ordered header list: `set` replaces any
/// same-name entry (case-insensitive), `delete` removes it.
fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(entry) = headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        entry.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

fn remove_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
}

/// Port of `buildBaseCodexHeaders`: model headers first, option headers
/// set/delete over them, then the fixed Codex auth headers.
fn build_base_codex_headers(
    init_headers: Option<&BTreeMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: Option<&str>,
    token: &str,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(init_headers) = init_headers {
        for (name, value) in init_headers {
            upsert_header(&mut headers, name, value);
        }
    }
    if let Some(additional_headers) = additional_headers {
        for (name, value) in additional_headers {
            if let Some(value) = value {
                upsert_header(&mut headers, name, value);
            } else {
                remove_header(&mut headers, name);
            }
        }
    }
    upsert_header(&mut headers, "Authorization", &format!("Bearer {token}"));
    if let Some(account_id) = account_id {
        upsert_header(&mut headers, "chatgpt-account-id", account_id);
    }
    upsert_header(&mut headers, "originator", "pi");
    upsert_header(
        &mut headers,
        "User-Agent",
        &crate::ai::session_resources::get_pi_user_agent(),
    );
    headers
}

/// Port of `buildWebSocketHeaders`.
fn build_websocket_headers(
    init_headers: Option<&BTreeMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: Option<&str>,
    token: &str,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut headers = build_base_codex_headers(init_headers, additional_headers, account_id, token);
    remove_header(&mut headers, "accept");
    remove_header(&mut headers, "content-type");
    remove_header(&mut headers, "OpenAI-Beta");
    remove_header(&mut headers, "openai-beta");
    upsert_header(
        &mut headers,
        "OpenAI-Beta",
        OPENAI_BETA_RESPONSES_WEBSOCKETS,
    );
    upsert_header(&mut headers, "x-client-request-id", request_id);
    upsert_header(&mut headers, "session-id", request_id);
    headers
}

/// Port of `compressRequestBodyZstd`: the zstd-compressed body bytes. The
/// Rust runtime always links zstd (the Node oracle path), so compression
/// never falls back to the uncompressed JSON.
fn compress_request_body_zstd(body_json: &str) -> Option<Vec<u8>> {
    zstd::bulk::compress(body_json.as_bytes(), REQUEST_COMPRESSION_ZSTD_LEVEL).ok()
}

/// Port of `isTerminalRateLimitError`.
fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    let patterns = [
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ];
    let lowered = error_text.to_lowercase();
    patterns
        .iter()
        .any(|pattern| lowered.contains(&pattern.to_lowercase()))
}

/// Port of `isRetryableError`.
fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    let lowered = error_text.to_lowercase();
    [
        "rate.?limit",
        "overloaded",
        "service.?unavailable",
        "upstream.?connect",
        "connection.?refused",
    ]
    .iter()
    .any(|pattern| regex_search(pattern, &lowered))
}

/// Minimal case-insensitive substring regex for the simple patterns used
/// here (`.` wildcards and `?` optional chars collapse to substring checks
/// over both spellings).
fn regex_search(pattern: &str, text: &str) -> bool {
    let literal = pattern.replace(".?", " ").replace('.', " ");
    let compact: String = literal.chars().filter(|ch| !ch.is_whitespace()).collect();
    let expanded = pattern.replace(".?", "").replace('.', "");
    text.contains(&compact) || text.contains(&expanded.to_lowercase())
}

/// Port of `getRetryAfterDelayMs`.
fn get_retry_after_delay_ms(headers: &[(String, String)], now_ms: i64) -> Option<f64> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    if let Some(retry_after_ms) = header("retry-after-ms")
        && let Ok(millis) = retry_after_ms.parse::<f64>()
        && millis.is_finite()
    {
        return Some(millis.max(0.0));
    }

    let retry_after = header("retry-after")?;
    if let Ok(seconds) = retry_after.parse::<f64>()
        && seconds.is_finite()
    {
        return Some((seconds * 1000.0).max(0.0));
    }
    if let Ok(date_ms) = httpdate::parse_http_date(&retry_after).map(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default()
    }) {
        return Some(((date_ms - now_ms) as f64).max(0.0));
    }
    None
}

/// Port of `buildRequestBody`.
#[allow(clippy::too_many_lines)]
pub fn build_request_body(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICodexResponsesOptions>,
    cache_session_id: Option<&str>,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let options = options.cloned().unwrap_or_default();
    let supports_strict_mode = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_strict_mode)
        .unwrap_or(true);
    let supports_openai_grammar_tools = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_open_ai_grammar_tools)
        .unwrap_or(false);
    let deferred_tools_mode = if model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_additional_tools)
        .unwrap_or(false)
    {
        Some(crate::ai::api::openai_responses_shared::DeferredToolsMode::AdditionalTools)
    } else if model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_tool_search)
        .unwrap_or(false)
    {
        Some(crate::ai::api::openai_responses_shared::DeferredToolsMode::ToolSearch)
    } else {
        None
    };
    let tool_placement = split_deferred_tools(context, deferred_tools_mode.is_some(), |name| {
        name.to_string()
    });
    let messages = convert_responses_messages(
        model,
        context,
        &codex_tool_call_providers(),
        Some(&ConvertResponsesMessagesOptions {
            include_system_prompt: Some(false),
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&tool_placement.deferred),
            deferred_tools_mode,
            tool_options: Some(
                crate::ai::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    strict: Some(None),
                    supports_strict_mode: Some(supports_strict_mode),
                    supports_openai_grammar_tools: Some(supports_openai_grammar_tools),
                    defer_loading: None,
                },
            ),
        }),
    )?;

    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": context.system_prompt.clone().unwrap_or_else(|| "You are a helpful assistant.".to_string()),
        "input": messages,
        "text": {"verbosity": options.text_verbosity.clone().unwrap_or_else(|| "low".to_string())},
        "include": ["reasoning.encrypted_content"],
        "tool_choice": options.tool_choice.clone().unwrap_or_else(|| "auto".to_string()),
        "parallel_tool_calls": true,
    });
    // TypeScript sets `prompt_cache_key: cacheSessionId`, which JSON.stringify
    // omits entirely when undefined; only send the key when present.
    if let Some(cache_session_id) = cache_session_id {
        body["prompt_cache_key"] = json!(cache_session_id);
    }

    if let Some(temperature) = options.base.temperature {
        body["temperature"] = json!(temperature);
    }

    if let Some(service_tier) = &options.service_tier {
        body["service_tier"] = json!(service_tier);
    }

    if !tool_placement.immediate.is_empty() {
        body["tools"] = Value::Array(convert_responses_tools(
            &tool_placement.immediate,
            Some(
                &crate::ai::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    strict: Some(None),
                    supports_strict_mode: Some(supports_strict_mode),
                    supports_openai_grammar_tools: Some(supports_openai_grammar_tools),
                    defer_loading: None,
                },
            ),
        )?);
    }

    if let Some(reasoning_effort) = &options.reasoning_effort {
        let key = match reasoning_effort.as_str() {
            "minimal" => Some(crate::ai::types::ModelThinkingLevel::Minimal),
            "low" => Some(crate::ai::types::ModelThinkingLevel::Low),
            "medium" => Some(crate::ai::types::ModelThinkingLevel::Medium),
            "high" => Some(crate::ai::types::ModelThinkingLevel::High),
            "xhigh" => Some(crate::ai::types::ModelThinkingLevel::Xhigh),
            "max" => Some(crate::ai::types::ModelThinkingLevel::Max),
            _ => None,
        };
        let effort = if reasoning_effort == "none" {
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::ai::types::ModelThinkingLevel::Off))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_string())
        } else {
            key.and_then(|key| {
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&key))
                    .cloned()
                    .flatten()
            })
            .unwrap_or_else(|| reasoning_effort.clone())
        };
        body["reasoning"] = json!({
            "effort": effort,
            "summary": options.reasoning_summary.clone().unwrap_or_else(|| "auto".to_string()),
        });
    }

    Ok(body)
}

/// Port of `resolveCodexServiceTier`.
fn resolve_codex_service_tier(
    response_service_tier: Option<&str>,
    request_service_tier: Option<&str>,
) -> Option<String> {
    if response_service_tier == Some("default")
        && matches!(request_service_tier, Some("flex") | Some("priority"))
    {
        return request_service_tier.map(str::to_string);
    }
    response_service_tier
        .map(str::to_string)
        .or_else(|| request_service_tier.map(str::to_string))
}

fn get_service_tier_cost_multiplier(model: &Model, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model.id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model: &Model) {
    let multiplier = get_service_tier_cost_multiplier(model, service_tier);
    if multiplier == 1.0 {
        return;
    }

    usage.cost.input = (f64::from(usage.cost.input) * multiplier).into();
    usage.cost.output = (f64::from(usage.cost.output) * multiplier).into();
    usage.cost.cache_read = (f64::from(usage.cost.cache_read) * multiplier).into();
    usage.cost.cache_write = (f64::from(usage.cost.cache_write) * multiplier).into();
    usage.cost.total = (f64::from(usage.cost.input)
        + f64::from(usage.cost.output)
        + f64::from(usage.cost.cache_read)
        + f64::from(usage.cost.cache_write))
    .into();
}

/// The unfold state threaded through the Codex SSE event stream.
type CodexSseState = (
    crate::ai::utils::sse::SseStream,
    bool,
    Arc<std::sync::Mutex<Option<bool>>>,
    Option<tokio_util::sync::CancellationToken>,
);

/// Terminal `Err("Request was aborted")` item carrying the unfold state.
fn aborted_codex_item(
    sse: crate::ai::utils::sse::SseStream,
    end_turn: Arc<std::sync::Mutex<Option<bool>>>,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> Option<(Result<Value, String>, CodexSseState)> {
    Some((
        Err("Request was aborted".to_string()),
        (sse, true, end_turn, signal),
    ))
}

/// Port of the `parseSSE` + `mapCodexEvents` async-generator pipeline: a lazy
/// stream of mapped Codex events fed to the shared Responses processor as
/// they arrive off the SSE body.
///
/// Like the TypeScript generators, consumption stops at the terminal
/// `response.done`/`response.completed`/`response.incomplete` event even when
/// the SSE body stays open, `__error__` frames surface as `Err` items, and an
/// aborted signal cancels pending body reads (surfacing "Request was
/// aborted"). A boolean `end_turn` on the terminal response is captured into
/// `end_turn` for the caller to apply once processing finishes.
fn codex_sse_event_stream(
    body: futures::stream::BoxStream<
        'static,
        Result<bytes::Bytes, crate::ai::utils::http::HttpFetchError>,
    >,
    signal: Option<tokio_util::sync::CancellationToken>,
    end_turn: Arc<std::sync::Mutex<Option<bool>>>,
) -> impl futures::Stream<Item = Result<Value, String>> {
    let sse = crate::ai::utils::sse::SseStream::new(body);
    futures::stream::unfold(
        (sse, false, end_turn, signal),
        |(mut sse, done, end_turn, signal)| async move {
            if done {
                return None;
            }
            loop {
                if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
                    return aborted_codex_item(sse, end_turn, signal);
                }
                let sse_event = match signal.as_ref() {
                    Some(token) => tokio::select! {
                        event = futures::StreamExt::next(&mut sse) => event,
                        () = token.cancelled() => {
                            return aborted_codex_item(sse, end_turn, signal);
                        }
                    },
                    None => futures::StreamExt::next(&mut sse).await,
                };
                if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
                    return aborted_codex_item(sse, end_turn, signal);
                }
                let sse_event = sse_event?;
                if sse_event.event.as_deref() == Some("__error__") {
                    return Some((Err(sse_event.data), (sse, true, end_turn, signal)));
                }
                let data = sse_event.data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };

                let Some(event_type) = event
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };

                if event_type == "error" {
                    let code = event
                        .get("code")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let message = event
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let detail = message
                        .clone()
                        .or_else(|| code.clone())
                        .unwrap_or_else(|| serde_json::to_string(&event).unwrap_or_default());
                    // Return the error through a synthetic error event so the shared
                    // processor surfaces it with the Codex prefix.
                    return Some((
                        Ok(json!({
                            "type": "error",
                            "code": code.unwrap_or_default(),
                            "message": format!("Codex error: {detail}"),
                        })),
                        (sse, true, end_turn, signal),
                    ));
                }

                if event_type == "response.failed" {
                    let code = event
                        .pointer("/response/error/code")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let message = event
                        .pointer("/response/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| "Codex response failed".to_string());
                    return Some((
                        Ok(json!({
                            "type": "error",
                            "code": code.unwrap_or_default(),
                            "message": message,
                        })),
                        (sse, true, end_turn, signal),
                    ));
                }

                if matches!(
                    event_type.as_str(),
                    "response.done" | "response.completed" | "response.incomplete"
                ) {
                    let mut event = event;
                    if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
                    {
                        if let Some(end_turn_value) = response.get("end_turn")
                            && end_turn_value.is_boolean()
                        {
                            *end_turn.lock().unwrap() = end_turn_value.as_bool();
                        }
                        // Normalize unknown statuses to undefined.
                        if let Some(status) = response.get("status")
                            && !matches!(
                                status.as_str(),
                                Some("completed")
                                    | Some("incomplete")
                                    | Some("failed")
                                    | Some("cancelled")
                                    | Some("queued")
                                    | Some("in_progress")
                            )
                        {
                            response.remove("status");
                        }
                    }
                    event["type"] = json!("response.completed");
                    return Some((Ok(event), (sse, true, end_turn, signal)));
                }

                return Some((Ok(event), (sse, false, end_turn, signal)));
            }
        },
    )
}

/// Port of the `parseWebSocket` + `mapCodexEvents` pipeline over a
/// [`WebSocketLike`](super::openai_codex_websocket::WebSocketLike) socket:
/// buffered socket events parsed as JSON frames and mapped exactly like the
/// SSE path, with transport/protocol/API failures recorded into `failure`
/// for the retry/fallback decision (TypeScript reads them off the thrown
/// error's class and `code`).
fn codex_websocket_event_stream(
    socket: Arc<dyn crate::ai::api::openai_codex_websocket::WebSocketLike>,
    signal: Option<tokio_util::sync::CancellationToken>,
    idle_timeout_ms: Option<u64>,
    end_turn: Arc<std::sync::Mutex<Option<bool>>>,
    failure: Arc<std::sync::Mutex<Option<CodexWsFailure>>>,
) -> impl futures::Stream<Item = Result<Value, String>> {
    type WsState = (
        Arc<dyn crate::ai::api::openai_codex_websocket::WebSocketLike>,
        Option<tokio_util::sync::CancellationToken>,
        Option<u64>,
        Arc<std::sync::Mutex<Option<bool>>>,
        Arc<std::sync::Mutex<Option<CodexWsFailure>>>,
        bool,
    );
    let record_failure = |failure: &Arc<std::sync::Mutex<Option<CodexWsFailure>>>,
                          error: CodexWsFailure|
     -> String {
        let mut slot = failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let message = error.to_string();
        if slot.is_none() {
            *slot = Some(error);
        }
        message
    };
    futures::stream::unfold(
        (
            socket,
            signal,
            idle_timeout_ms,
            end_turn,
            failure,
            false, // saw_completion
        ) as WsState,
        move |(socket, signal, idle_timeout_ms, end_turn, failure, saw_completion)| {
            async move {
                loop {
                    // `mapCodexEvents` returns after the terminal response
                    // event; the mapped stream ends with it.
                    if saw_completion {
                        return None;
                    }
                    if let Some(token) = signal.as_ref()
                        && token.is_cancelled()
                    {
                        let message = record_failure(
                            &failure,
                            CodexWsFailure::transport("Request was aborted"),
                        );
                        return Some((
                            Err(message),
                            (
                                socket,
                                signal,
                                idle_timeout_ms,
                                end_turn,
                                failure,
                                saw_completion,
                            ),
                        ));
                    }
                    let next = socket.next_event();
                    let event = tokio::select! {
                        event = next => event,
                        () = async {
                            match idle_timeout_ms.filter(|timeout| *timeout > 0) {
                                Some(timeout) => tokio::time::sleep(std::time::Duration::from_millis(timeout)).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            socket.close_silently(1000, "idle_timeout");
                            let timeout = idle_timeout_ms.unwrap_or_default();
                            let message = record_failure(
                                &failure,
                                CodexWsFailure::transport(format!(
                                    "WebSocket idle timeout after {timeout}ms"
                                )),
                            );
                            return Some((Err(message), (socket, signal, idle_timeout_ms, end_turn, failure, saw_completion)));
                        }
                        () = async {
                            match &signal {
                                Some(token) => token.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            let message = record_failure(
                                &failure,
                                CodexWsFailure::transport("Request was aborted"),
                            );
                            return Some((Err(message), (socket, signal, idle_timeout_ms, end_turn, failure, saw_completion)));
                        }
                    };
                    match event {
                        Some(WebSocketEvent::Message(text)) => {
                            let parsed: Result<Value, _> = serde_json::from_str(&text);
                            let event = match parsed {
                                Ok(event) => event,
                                Err(cause) => {
                                    let message = record_failure(
                                        &failure,
                                        CodexWsFailure::protocol(format!(
                                            "Invalid Codex WebSocket JSON: {cause}"
                                        )),
                                    );
                                    return Some((
                                        Err(message),
                                        (
                                            socket,
                                            signal,
                                            idle_timeout_ms,
                                            end_turn,
                                            failure,
                                            saw_completion,
                                        ),
                                    ));
                                }
                            };
                            let Some(event_type) = event
                                .get("type")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                            else {
                                continue;
                            };
                            if event_type == "error" {
                                // Port of `extractCodexEventError`.
                                let nested = event.get("error").filter(|value| value.is_object());
                                let code = event
                                    .get("code")
                                    .and_then(Value::as_str)
                                    .or_else(|| {
                                        nested
                                            .and_then(|nested| nested.get("code"))
                                            .and_then(Value::as_str)
                                    })
                                    .map(str::to_string);
                                let message = event
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .or_else(|| {
                                        nested
                                            .and_then(|nested| nested.get("message"))
                                            .and_then(Value::as_str)
                                    })
                                    .map(str::to_string);
                                let detail =
                                    message.clone().or_else(|| code.clone()).unwrap_or_else(|| {
                                        serde_json::to_string(&event).unwrap_or_default()
                                    });
                                let message = record_failure(
                                    &failure,
                                    CodexWsFailure::api(format!("Codex error: {detail}"), code),
                                );
                                return Some((
                                    Err(message),
                                    (
                                        socket,
                                        signal,
                                        idle_timeout_ms,
                                        end_turn,
                                        failure,
                                        saw_completion,
                                    ),
                                ));
                            }
                            if event_type == "response.failed" {
                                let code = event
                                    .pointer("/response/error/code")
                                    .and_then(Value::as_str)
                                    .map(str::to_string);
                                let message = event
                                    .pointer("/response/error/message")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| "Codex response failed".to_string());
                                let message =
                                    record_failure(&failure, CodexWsFailure::api(message, code));
                                return Some((
                                    Err(message),
                                    (
                                        socket,
                                        signal,
                                        idle_timeout_ms,
                                        end_turn,
                                        failure,
                                        saw_completion,
                                    ),
                                ));
                            }
                            if matches!(
                                event_type.as_str(),
                                "response.done" | "response.completed" | "response.incomplete"
                            ) {
                                let mut event = event;
                                if let Some(response) =
                                    event.get_mut("response").and_then(Value::as_object_mut)
                                {
                                    if let Some(end_turn_value) = response.get("end_turn")
                                        && end_turn_value.is_boolean()
                                    {
                                        *end_turn.lock().unwrap() = end_turn_value.as_bool();
                                    }
                                    if let Some(status) = response.get("status")
                                        && !matches!(
                                            status.as_str(),
                                            Some("completed")
                                                | Some("incomplete")
                                                | Some("failed")
                                                | Some("cancelled")
                                                | Some("queued")
                                                | Some("in_progress")
                                        )
                                    {
                                        response.remove("status");
                                    }
                                }
                                event["type"] = json!("response.completed");
                                return Some((
                                    Ok(event),
                                    (socket, signal, idle_timeout_ms, end_turn, failure, true),
                                ));
                            }
                            return Some((
                                Ok(event),
                                (
                                    socket,
                                    signal,
                                    idle_timeout_ms,
                                    end_turn,
                                    failure,
                                    saw_completion,
                                ),
                            ));
                        }
                        Some(WebSocketEvent::Error(message)) => {
                            let error_message =
                                record_failure(&failure, CodexWsFailure::transport(message));
                            return Some((
                                Err(error_message),
                                (
                                    socket,
                                    signal,
                                    idle_timeout_ms,
                                    end_turn,
                                    failure,
                                    saw_completion,
                                ),
                            ));
                        }
                        Some(WebSocketEvent::Close { code, reason }) => {
                            if saw_completion {
                                return None;
                            }
                            let error_message = record_failure(
                                &failure,
                                CodexWsFailure::transport(
                                    crate::ai::api::openai_codex_websocket::close_error_message(
                                        code,
                                        reason.as_deref(),
                                    ),
                                ),
                            );
                            return Some((
                                Err(error_message),
                                (
                                    socket,
                                    signal,
                                    idle_timeout_ms,
                                    end_turn,
                                    failure,
                                    saw_completion,
                                ),
                            ));
                        }
                        Some(WebSocketEvent::Open) => continue,
                        None => {
                            if saw_completion {
                                return None;
                            }
                            let error_message = record_failure(
                                &failure,
                                CodexWsFailure::transport(
                                    "WebSocket stream closed before response.completed",
                                ),
                            );
                            return Some((
                                Err(error_message),
                                (
                                    socket,
                                    signal,
                                    idle_timeout_ms,
                                    end_turn,
                                    failure,
                                    saw_completion,
                                ),
                            ));
                        }
                    }
                }
            }
        },
    )
}

/// Port of the `stream` stream function (SSE transport).
#[allow(clippy::too_many_lines)]
pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICodexResponsesOptions>,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let model = model.clone();
    let context = context.clone();
    let options = options.cloned();
    let producer = stream.clone();
    tokio::spawn(async move {
        let mut output = crate::ai::types::AssistantMessage {
            role: crate::ai::types::RoleAssistant,
            content: Vec::new(),
            api: "openai-codex-responses".to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Default::default(),
            stop_reason: crate::ai::types::StopReason::Pending,
            timestamp: crate::ai::auth::resolve::now_millis(),
            ..Default::default()
        };

        let result = run_stream(&model, &context, options.as_ref(), &mut output, &producer).await;
        if let Err(error) = result {
            let aborted = options
                .as_ref()
                .and_then(|options| options.base.base.signal.as_ref())
                .is_some_and(|token| token.is_cancelled());
            output.stop_reason = if aborted {
                crate::ai::types::StopReason::Aborted
            } else {
                crate::ai::types::StopReason::Error
            };
            let formatted = format_provider_error(
                &normalize_provider_error(ProviderErrorParts {
                    status: None,
                    body: None,
                    message: error,
                }),
                None,
            );
            output.error_message = Some(formatted);
            producer.push(crate::ai::types::AssistantMessageEvent::Error {
                reason: if output.stop_reason == crate::ai::types::StopReason::Aborted {
                    crate::ai::types::ErrorReason::Aborted
                } else {
                    crate::ai::types::ErrorReason::Error
                },
                error: output.clone(),
            });
            producer.end(None);
        }
    });
    stream
}

/// Port of `processWebSocketStream`: acquire (or reuse) the session socket,
/// send the `response.create` frame (delta-only when a cached continuation
/// matches), and feed the mapped events through the shared Responses
/// processor, starting the message stream on the first event.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn process_web_socket_stream(
    model: &Model,
    body: Value,
    headers: Vec<(String, String)>,
    output: &mut crate::ai::types::AssistantMessage,
    producer: &AssistantMessageEventStream,
    on_start: &Arc<dyn Fn() + Send + Sync>,
    idle_timeout_ms: Option<u64>,
    websocket_connect_timeout_ms: Option<u64>,
    cache_session_id: Option<&str>,
    account_id: &str,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    options: Option<&OpenAICodexResponsesOptions>,
    transport: Transport,
) -> Result<(), CodexWsFailure> {
    let signal = options.and_then(|options| options.base.base.signal.clone());
    let url = resolve_codex_websocket_url(Some(&model.base_url));
    let acquired: AcquiredWebSocket = acquire_web_socket(
        &url,
        headers,
        cache_session_id,
        account_id,
        signal.clone(),
        websocket_connect_timeout_ms,
    )
    .await?;
    let mut keep_connection = true;
    let use_cached_context = matches!(transport, Transport::WebsocketCached | Transport::Auto);
    let request_body = if use_cached_context {
        acquired.cached_request_body(&body)
    } else {
        body.clone()
    };

    if let Some(session_id) = cache_session_id {
        let mut stats =
            crate::ai::api::openai_codex_websocket::get_or_create_websocket_debug_stats(session_id);
        stats.requests += 1;
        if acquired.reused {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if use_cached_context {
            stats.cached_context_requests += 1;
        }
        if request_body.get("store") == Some(&json!(true)) {
            stats.store_true_requests += 1;
        }
        let input_items = request_body
            .get("input")
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as u64;
        stats.last_input_items = input_items;
        let previous_response_id = request_body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        if let Some(previous_response_id) = previous_response_id {
            stats.delta_requests += 1;
            stats.last_delta_input_items = Some(input_items);
            stats.last_previous_response_id = Some(previous_response_id);
        } else {
            stats.full_context_requests += 1;
            stats.last_delta_input_items = None;
            stats.last_previous_response_id = None;
        }
        crate::ai::api::openai_codex_websocket::write_websocket_debug_stats(session_id, stats);
    }

    let result = run_web_socket_request(
        model,
        acquired_socket(&acquired),
        request_body,
        body.clone(),
        output,
        producer,
        on_start,
        idle_timeout_ms,
        signal,
        use_cached_context,
        grammar_tool_input_properties,
        options,
    )
    .await;
    match result {
        Ok(continuation) => {
            if let Some((full_body, response_id, response_items)) = continuation {
                acquired.save_continuation(full_body, response_id, response_items);
            }
            acquired.release(keep_connection);
            Ok(())
        }
        Err(failure) => {
            acquired.clear_continuation();
            keep_connection = false;
            acquired.release(keep_connection);
            Err(failure)
        }
    }
}

fn acquired_socket(
    acquired: &AcquiredWebSocket,
) -> Arc<dyn crate::ai::api::openai_codex_websocket::WebSocketLike> {
    Arc::clone(&acquired.socket)
}

#[allow(clippy::too_many_arguments)]
async fn run_web_socket_request(
    model: &Model,
    socket: Arc<dyn crate::ai::api::openai_codex_websocket::WebSocketLike>,
    request_body: Value,
    full_body: Value,
    output: &mut crate::ai::types::AssistantMessage,
    producer: &AssistantMessageEventStream,
    on_start: &Arc<dyn Fn() + Send + Sync>,
    idle_timeout_ms: Option<u64>,
    signal: Option<tokio_util::sync::CancellationToken>,
    use_cached_context: bool,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    options: Option<&OpenAICodexResponsesOptions>,
) -> Result<Option<(Value, String, Value)>, CodexWsFailure> {
    // The `response.create` frame spreads the body after `type`.
    let mut frame = Map::new();
    frame.insert("type".to_string(), json!("response.create"));
    if let Some(object) = request_body.as_object() {
        for (key, value) in object {
            frame.insert(key.clone(), value.clone());
        }
    }
    socket.send_text(&Value::Object(frame).to_string());

    let model_owned = model.clone();
    let stream_options = ResponsesStreamOptions {
        service_tier: options.and_then(|options| options.service_tier.clone()),
        grammar_tool_input_properties: Some(grammar_tool_input_properties.clone()),
        resolve_service_tier: Some(Box::new(
            |response_tier: Option<&str>, request_tier: Option<&str>| {
                resolve_codex_service_tier(response_tier, request_tier)
            },
        )),
        apply_service_tier_pricing: Some(Box::new(
            move |usage: &mut Usage, service_tier: Option<&str>| {
                apply_service_tier_pricing(usage, service_tier, &model_owned);
            },
        )),
    };
    let end_turn: Arc<std::sync::Mutex<Option<bool>>> = Arc::new(std::sync::Mutex::new(None));
    let failure: Arc<std::sync::Mutex<Option<CodexWsFailure>>> =
        Arc::new(std::sync::Mutex::new(None));
    let mapped = codex_websocket_event_stream(
        socket,
        signal.clone(),
        idle_timeout_ms,
        Arc::clone(&end_turn),
        Arc::clone(&failure),
    );
    // Port of `startWebSocketOutputOnFirstEvent`.
    let on_start = Arc::clone(on_start);
    let mapped = std::pin::pin!(mapped);
    let mapped_with_start = futures::stream::unfold(
        (mapped, on_start, false),
        |(mut mapped, on_start, started)| async move {
            match futures::StreamExt::next(&mut mapped).await {
                Some(item) => {
                    // The TypeScript generator throws (never yields) on
                    // error frames, so only mapped events start the stream.
                    let started = if started || item.is_err() {
                        true
                    } else {
                        on_start();
                        true
                    };
                    Some((item, (mapped, on_start, started)))
                }
                None => None,
            }
        },
    );
    let processed = process_responses_stream(
        mapped_with_start,
        output,
        producer,
        model,
        Some(&stream_options),
    )
    .await;
    if let Some(end_turn) = end_turn.lock().unwrap().take() {
        output.end_turn = Some(end_turn);
    }
    processed.map_err(|message| {
        let mut slot = failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.take()
            .unwrap_or_else(|| CodexWsFailure::transport(message))
    })?;

    let aborted = signal.as_ref().is_some_and(|token| token.is_cancelled());
    if aborted {
        return Ok(None);
    }
    if use_cached_context && let Some(response_id) = output.response_id.clone() {
        let response_items = convert_responses_messages(
            model,
            &Context {
                messages: vec![crate::ai::types::Message::Assistant(Box::new(
                    output.clone(),
                ))],
                ..Default::default()
            },
            &codex_tool_call_providers(),
            Some(&ConvertResponsesMessagesOptions {
                include_system_prompt: Some(false),
                grammar_tool_input_properties: Some(grammar_tool_input_properties),
                deferred_tools: None,
                deferred_tools_mode: None,
                tool_options: None,
            }),
        )
        .map_err(CodexWsFailure::protocol)?;
        let filtered: Vec<Value> = response_items
            .into_iter()
            .filter(|item| {
                !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call_output") | Some("custom_tool_call_output")
                )
            })
            .collect();
        return Ok(Some((full_body, response_id, Value::Array(filtered))));
    }
    Ok(None)
}

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICodexResponsesOptions>,
    output: &mut crate::ai::types::AssistantMessage,
    producer: &AssistantMessageEventStream,
) -> Result<(), String> {
    let api_key = get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    )?;

    let account_id = extract_account_id(&api_key);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
    )?;
    let cache_session_id =
        if options.and_then(|options| options.base.cache_retention) == Some(CacheRetention::None) {
            None
        } else {
            options.and_then(|options| options.base.session_id.clone())
        };
    let codex_session_id = clamp_openai_prompt_cache_key(cache_session_id.as_deref());
    let mut body = build_request_body(
        model,
        context,
        options,
        codex_session_id.as_deref(),
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = options.and_then(|options| options.base.base.on_payload.as_ref())
        && let Some(next_body) = on_payload(body.clone(), model).await
    {
        body = next_body;
    }

    // Port of the header assembly shared by both transports.
    let model_headers = model.headers.as_ref();
    let options_headers = options.and_then(|options| options.base.base.headers.as_ref());
    let websocket_request_id = codex_session_id
        .clone()
        .unwrap_or_else(crate::ai::utils::uuid::uuidv7);
    let websocket_headers = build_websocket_headers(
        model_headers,
        options_headers,
        account_id.as_deref(),
        &api_key,
        websocket_request_id.as_str(),
    );
    let body_json = serde_json::to_string(&body).unwrap_or_default();
    let http_timeout_ms = options.and_then(|options| options.base.base.timeout_ms);
    let websocket_connect_timeout_ms = options
        .and_then(|options| options.base.websocket_connect_timeout_ms)
        .unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);
    let transport = options
        .and_then(|options| options.base.transport)
        .unwrap_or(Transport::Auto);
    let signal = options.and_then(|options| options.base.base.signal.clone());
    // Shared across WebSocket attempts and the SSE fallback: the start
    // event fires once per stream (the TypeScript `startEmitted` flag).
    let start_emitted = Arc::new(AtomicBool::new(false));

    let websocket_disabled_for_session = transport != Transport::Sse
        && is_websocket_sse_fallback_active(cache_session_id.as_deref());
    if websocket_disabled_for_session {
        record_websocket_sse_fallback(cache_session_id.as_deref());
    }

    let mut websocket_succeeded = false;
    if transport != Transport::Sse && !websocket_disabled_for_session {
        let websocket_started = Arc::new(AtomicBool::new(false));
        let mut retried_websocket_connection_limit = false;
        let mut retried_missing_websocket_continuation = false;
        loop {
            websocket_started.store(false, Ordering::SeqCst);
            let on_start: Arc<dyn Fn() + Send + Sync> = {
                let websocket_started = Arc::clone(&websocket_started);
                let start_emitted = Arc::clone(&start_emitted);
                let producer = producer.clone();
                let partial = output.clone();
                Arc::new(move || {
                    websocket_started.store(true, Ordering::SeqCst);
                    if !start_emitted.swap(true, Ordering::SeqCst) {
                        producer.push(crate::ai::types::AssistantMessageEvent::Start {
                            partial: partial.clone(),
                        });
                    }
                })
            };
            let attempt = process_web_socket_stream(
                model,
                body.clone(),
                websocket_headers.clone(),
                output,
                producer,
                &on_start,
                http_timeout_ms,
                Some(websocket_connect_timeout_ms),
                cache_session_id.as_deref(),
                account_id.as_deref().unwrap_or_default(),
                &grammar_tool_input_properties,
                options,
                transport,
            )
            .await;
            match attempt {
                Ok(()) => {
                    websocket_succeeded = true;
                    break;
                }
                Err(failure) => {
                    let aborted = signal.as_ref().is_some_and(|token| token.is_cancelled());
                    let started = websocket_started.load(Ordering::SeqCst);
                    let connection_limit_before_start =
                        !started && failure.is_connection_limit_reached();
                    let previous_response_not_found = failure.is_previous_response_not_found();
                    if !aborted
                        && previous_response_not_found
                        && !retried_missing_websocket_continuation
                    {
                        retried_missing_websocket_continuation = true;
                        continue;
                    }
                    if !aborted
                        && connection_limit_before_start
                        && !retried_websocket_connection_limit
                    {
                        retried_websocket_connection_limit = true;
                        continue;
                    }
                    if aborted || (failure.is_non_transport() && !connection_limit_before_start) {
                        return Err(failure.message);
                    }
                    let mut details = Map::new();
                    details.insert("configuredTransport".to_string(), json!(transport.as_str()));
                    if !started {
                        details.insert("fallbackTransport".to_string(), json!("sse"));
                    }
                    details.insert("eventsEmitted".to_string(), json!(started));
                    details.insert(
                        "phase".to_string(),
                        json!(if started {
                            "after_message_stream_start"
                        } else {
                            "before_message_stream_start"
                        }),
                    );
                    details.insert("requestBytes".to_string(), json!(body_json.len()));
                    append_assistant_message_diagnostic(
                        output,
                        create_assistant_message_diagnostic(
                            "provider_transport_failure",
                            &failure,
                            Some(Value::Object(details)),
                        ),
                    );
                    record_websocket_failure(cache_session_id.as_deref(), &failure);
                    if started {
                        return Err(failure.message);
                    }
                    record_websocket_sse_fallback(cache_session_id.as_deref());
                    break;
                }
            }
        }
        if websocket_succeeded {
            let signal = options.and_then(|options| options.base.base.signal.clone());
            if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
                return Err("Request was aborted".to_string());
            }
            if output.stop_reason == crate::ai::types::StopReason::Pending {
                return Err("Codex stream ended without a stop reason".to_string());
            }
            if output.stop_reason == crate::ai::types::StopReason::Error
                || output.stop_reason == crate::ai::types::StopReason::Aborted
            {
                return Err(output
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "An unknown error occurred".to_string()));
            }
            let reason = match output.stop_reason {
                crate::ai::types::StopReason::Length => crate::ai::types::DoneReason::Length,
                crate::ai::types::StopReason::ToolUse => crate::ai::types::DoneReason::ToolUse,
                crate::ai::types::StopReason::Deferred => crate::ai::types::DoneReason::Deferred,
                _ => crate::ai::types::DoneReason::Stop,
            };
            producer.push(crate::ai::types::AssistantMessageEvent::Done {
                reason,
                message: output.clone(),
            });
            producer.end(None);
            return Ok(());
        }
    }

    // Port of `buildSSEHeaders`: base Codex headers plus the SSE-only
    // OpenAI-Beta value and session affinity headers.
    let mut headers = build_base_codex_headers(
        model_headers,
        options_headers,
        account_id.as_deref(),
        &api_key,
    );
    upsert_header(&mut headers, "OpenAI-Beta", "responses=experimental");
    upsert_header(&mut headers, "accept", "text/event-stream");
    upsert_header(&mut headers, "content-type", "application/json");
    if let Some(session_id) = &codex_session_id {
        upsert_header(&mut headers, "session-id", session_id);
        upsert_header(&mut headers, "x-client-request-id", session_id);
    }

    // Compress the request body once for the SSE path. The Codex backend
    // decodes Content-Encoding: zstd; the WebSocket transport above sends the
    // uncompressed JSON frame, matching the official Codex client.
    let compressed_body = compress_request_body_zstd(&body_json);
    if compressed_body.is_some() {
        upsert_header(&mut headers, "content-encoding", "zstd");
    }
    let sse_body = compressed_body
        .map(|bytes| HttpBody::Bytes(bytes::Bytes::from(bytes)))
        .unwrap_or_else(|| HttpBody::Text(body_json.clone()));

    let fetch = options
        .and_then(|options| options.base.base.fetch.clone())
        .unwrap_or_else(default_fetch);
    let url = resolve_codex_url(Some(&model.base_url));
    let request = HttpRequest {
        signal: options.and_then(|options| options.base.base.signal.clone()),
        method: HttpMethod::Post,
        url,
        headers,
        body: sse_body,
    };

    // Port of the SSE retry loop: bounded attempts, retryable statuses, and
    // Retry-After handling with the terminal-rate-limit exclusion.
    let max_retries = options
        .and_then(|options| options.base.base.max_retries)
        .unwrap_or(DEFAULT_MAX_RETRIES);
    // Port of the `AbortSignal.timeout(httpTimeoutMs)` applied to the SSE
    // fetch: bounds the wait for response headers only (body reads are
    // cancelled through the caller's signal).
    let header_timeout_ms = options
        .and_then(|options| options.base.base.timeout_ms)
        .filter(|timeout_ms| *timeout_ms > 0);
    let now_ms = || crate::ai::auth::resolve::now_millis();
    let mut response = None;
    'attempts: for attempt in 0..=max_retries {
        let signal = options.and_then(|options| options.base.base.signal.clone());
        if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err("Request was aborted".to_string());
        }

        let current = retry_provider_request(
            || {
                let fetch = Arc::clone(&fetch);
                let request = request.clone();
                async move {
                    let pending = fetch.fetch(request);
                    if let Some(timeout_ms) = header_timeout_ms {
                        tokio::select! {
                            result = pending => result.map_err(|error| {
                                crate::ai::utils::provider_retry::ProviderHttpError::new(
                                    error.to_string(),
                                    None,
                                    Vec::new(),
                                )
                            }),
                            () = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                                Err(crate::ai::utils::provider_retry::ProviderHttpError::new(
                                    format!("Codex SSE response headers timed out after {timeout_ms}ms"),
                                    None,
                                    Vec::new(),
                                ))
                            }
                        }
                    } else {
                        pending.await.map_err(|error| {
                            crate::ai::utils::provider_retry::ProviderHttpError::new(
                                error.to_string(),
                                None,
                                Vec::new(),
                            )
                        })
                    }
                }
            },
            crate::ai::utils::provider_retry::ProviderRetryOptions {
                max_retries: Some(0),
                on_retry: options.and_then(|options| options.base.base.on_retry.clone()),
                max_retry_delay_ms: options
                    .and_then(|options| options.base.base.max_retry_delay_ms),
                signal: signal.clone(),
            },
        )
        .await;

        match current {
            Ok(current) => {
                let error_headers = current.headers.clone();
                let response_headers: BTreeMap<String, String> = current
                    .headers
                    .iter()
                    .map(|(name, value)| (name.to_lowercase(), value.clone()))
                    .collect();
                if let Some(on_response) =
                    options.and_then(|options| options.base.base.on_response.as_ref())
                {
                    on_response(
                        &crate::ai::types::ProviderResponse {
                            status: current.status,
                            headers: response_headers,
                        },
                        model,
                    )
                    .await;
                }

                if (200..300).contains(&current.status) {
                    response = Some(current);
                    break 'attempts;
                }

                let error_status = current.status;
                let error_text = crate::ai::utils::http::collect_text(current).await;
                if attempt < max_retries && is_retryable_error(error_status, &error_text) {
                    let retry_after_delay_ms = get_retry_after_delay_ms(&error_headers, now_ms());
                    let delay_ms = match retry_after_delay_ms {
                        Some(delay) => {
                            let max_delay = options
                                .and_then(|options| options.base.base.max_retry_delay_ms)
                                .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
                            if max_delay > 0 && delay > max_delay as f64 {
                                return Err(format!(
                                    "Server requested {}s retry delay (max: {}s)",
                                    (delay / 1000.0).ceil(),
                                    (max_delay as f64 / 1000.0).ceil(),
                                ));
                            }
                            delay as u64
                        }
                        None => BASE_DELAY_MS * 2u64.pow(attempt),
                    };
                    abortable_sleep(delay_ms, signal.as_ref()).await?;
                    continue;
                }

                return Err(format_provider_error(
                    &normalize_provider_error(ProviderErrorParts {
                        status: Some(error_status),
                        body: Some(error_text),
                        message: format!("{} status code", error_status),
                    }),
                    None,
                ));
            }
            Err(error) => {
                if error.message == "Request was aborted" {
                    return Err("Request was aborted".to_string());
                }
                if attempt < max_retries && !error.message.contains("usage limit") {
                    abortable_sleep(BASE_DELAY_MS * 2u64.pow(attempt), signal.as_ref()).await?;
                    continue;
                }
                return Err(error.message);
            }
        }
    }

    let response = response.ok_or_else(|| "Failed after retries".to_string())?;
    if !start_emitted.swap(true, Ordering::SeqCst) {
        producer.push(crate::ai::types::AssistantMessageEvent::Start {
            partial: output.clone(),
        });
    }

    let model_owned = model.clone();
    let stream_options = ResponsesStreamOptions {
        service_tier: options.and_then(|options| options.service_tier.clone()),
        grammar_tool_input_properties: Some(grammar_tool_input_properties),
        resolve_service_tier: Some(Box::new(
            |response_tier: Option<&str>, request_tier: Option<&str>| {
                resolve_codex_service_tier(response_tier, request_tier)
            },
        )),
        apply_service_tier_pricing: Some(Box::new(
            move |usage: &mut Usage, service_tier: Option<&str>| {
                apply_service_tier_pricing(usage, service_tier, &model_owned);
            },
        )),
    };
    // Lazily feed mapped Codex SSE events into the shared processor so the
    // pipeline finishes at the terminal response event even while the SSE
    // body stays open, and so body-read errors/aborts surface mid-stream.
    let signal = options.and_then(|options| options.base.base.signal.clone());
    let end_turn: Arc<std::sync::Mutex<Option<bool>>> = Arc::new(std::sync::Mutex::new(None));
    let mapped = codex_sse_event_stream(response.body, signal, Arc::clone(&end_turn));
    let processed =
        process_responses_stream(mapped, output, producer, model, Some(&stream_options)).await;
    if let Some(end_turn) = end_turn.lock().unwrap().take() {
        output.end_turn = Some(end_turn);
    }
    processed?;

    let signal = options.and_then(|options| options.base.base.signal.clone());
    if signal.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err("Request was aborted".to_string());
    }

    if output.stop_reason == crate::ai::types::StopReason::Pending {
        return Err("Codex stream ended without a stop reason".to_string());
    }
    if output.stop_reason == crate::ai::types::StopReason::Error
        || output.stop_reason == crate::ai::types::StopReason::Aborted
    {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }

    let reason = match output.stop_reason {
        crate::ai::types::StopReason::Length => crate::ai::types::DoneReason::Length,
        crate::ai::types::StopReason::ToolUse => crate::ai::types::DoneReason::ToolUse,
        crate::ai::types::StopReason::Deferred => crate::ai::types::DoneReason::Deferred,
        _ => crate::ai::types::DoneReason::Stop,
    };
    producer.push(crate::ai::types::AssistantMessageEvent::Done {
        reason,
        message: output.clone(),
    });
    producer.end(None);
    Ok(())
}

async fn abortable_sleep(
    ms: u64,
    signal: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), String> {
    match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err("Request was aborted".to_string());
            }
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(ms)) => Ok(()),
                _ = signal.cancelled() => Err("Request was aborted".to_string()),
            }
        }
        None => {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(())
        }
    }
}

/// Port of `streamSimple`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if get_client_api_key(
        &model.provider,
        options.and_then(|options| options.base.base.api_key.as_deref()),
        options.and_then(|options| options.base.base.headers.as_ref()),
    )
    .is_err()
    {
        return crate::ai::api::openai_completions::error_stream(
            model,
            &format!("No API key for provider: {}", model.provider),
        );
    }

    let options = options.cloned().unwrap_or_default();
    let base = build_base_options(
        model,
        context,
        Some(&options),
        options.base.base.api_key.as_deref(),
    );
    let clamped_reasoning = options.reasoning.map(|reasoning| {
        clamp_thinking_level(model, crate::ai::types::ModelThinkingLevel::from(reasoning))
    });
    let reasoning_effort = clamped_reasoning
        .and_then(|level| level.as_thinking_level())
        .map(|level| {
            match level {
                ThinkingLevel::Minimal => "minimal",
                ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High => "high",
                ThinkingLevel::Xhigh => "xhigh",
                ThinkingLevel::Max => "max",
            }
            .to_string()
        });

    stream(
        model,
        context,
        Some(&OpenAICodexResponsesOptions {
            base,
            reasoning_effort,
            ..Default::default()
        }),
    )
}
