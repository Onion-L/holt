//! Port of `pi-core/ai/src/providers/faux.ts`: the faux provider used by
//! tests, streaming scripted responses with usage estimation, prompt-cache
//! simulation, and deferred-response support.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::ai::auth::resolve::ModelsError;
use crate::ai::models::{BasicProvider, CreateProviderOptions, ProviderStreams};
use crate::ai::providers::builtin::organization_for_provider;
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DeferredHandle, DoneReason,
    ErrorReason, Message, Model, ProviderRequestOptions, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingContent, ToolCall, Usage, UsageCost,
};
use crate::ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

const DEFAULT_API: &str = "faux";
const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";
const DEFAULT_MODEL_NAME: &str = "Faux Model";
const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

/// Port of `FauxModelDefinition`.
#[derive(Clone, Debug, Default)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<crate::ai::types::ModelInput>>,
    pub cost: Option<crate::ai::types::ModelCost>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// Port of `FauxProviderState`.
#[derive(Clone, Debug, Default)]
pub struct FauxProviderState {
    pub call_count: u64,
    pub deferred_fetch_count: u64,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

/// A scripted response: either a fixed message or a factory.
pub type FauxResponseFactory = Arc<
    dyn Fn(
            &Context,
            Option<&SimpleStreamOptions>,
            &FauxProviderState,
            &Model,
        ) -> Result<AssistantMessage, String>
        + Send
        + Sync,
>;

/// Port of `FauxResponseStep`.
#[derive(Clone)]
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(FauxResponseFactory),
}

/// Port of `RegisterFauxProviderOptions`.
#[derive(Clone, Default)]
pub struct RegisterFauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Vec<FauxModelDefinition>,
    pub deferred: Option<FauxDeferredOptions>,
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<FauxTokenSize>,
}

/// Deferred simulation options.
#[derive(Clone, Copy, Debug, Default)]
pub struct FauxDeferredOptions {
    /// Number of fetches that return the original handle before the scripted
    /// response becomes ready.
    pub pending_fetches: Option<u32>,
    pub poll_after_ms: Option<u64>,
}

/// Token size range for chunk splitting.
#[derive(Clone, Copy, Debug, Default)]
pub struct FauxTokenSize {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as f64 / 4.0).ceil() as u64
}

fn random_id(prefix: &str) -> String {
    format!("{}:{}", prefix, crate::ai::utils::uuid::uuidv7())
}

/// Port of `fauxText`.
pub fn faux_text(text: impl Into<String>) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        ..Default::default()
    })
}

/// Port of `fauxThinking`.
pub fn faux_thinking(thinking: impl Into<String>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: thinking.into(),
        ..Default::default()
    })
}

/// Port of `fauxToolCall`.
pub fn faux_tool_call(
    name: &str,
    arguments: serde_json::Value,
    id: Option<String>,
) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        content_type: Default::default(),
        id: id.unwrap_or_else(|| random_id("tool")),
        name: name.to_string(),
        arguments: serde_json::from_value(arguments).unwrap_or_default(),
        ..Default::default()
    })
}

/// Content accepted by `fauxAssistantMessage`.
pub enum FauxContent {
    Text(String),
    Blocks(Vec<AssistantContent>),
}

impl From<&str> for FauxContent {
    fn from(value: &str) -> Self {
        FauxContent::Text(value.to_string())
    }
}

impl From<String> for FauxContent {
    fn from(value: String) -> Self {
        FauxContent::Text(value)
    }
}

impl From<Vec<AssistantContent>> for FauxContent {
    fn from(value: Vec<AssistantContent>) -> Self {
        FauxContent::Blocks(value)
    }
}

impl From<AssistantContent> for FauxContent {
    fn from(value: AssistantContent) -> Self {
        FauxContent::Blocks(vec![value])
    }
}

/// Options accepted by `fauxAssistantMessage`.
#[derive(Default)]
pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub response_id: Option<String>,
    pub timestamp: Option<i64>,
    /// Overrides used by the faux provider internals.
    pub api: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Usage>,
}

/// Port of `fauxAssistantMessage`.
pub fn faux_assistant_message(
    content: impl Into<FauxContent>,
    options: FauxMessageOptions,
) -> AssistantMessage {
    let content = match content.into() {
        FauxContent::Text(text) => vec![faux_text(text)],
        FauxContent::Blocks(blocks) => blocks,
    };
    AssistantMessage {
        role: crate::ai::types::RoleAssistant,
        content,
        api: options.api.unwrap_or_else(|| DEFAULT_API.to_string()),
        provider: options
            .provider
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
        model: options
            .model
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string()),
        usage: options.usage.unwrap_or_default(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        deferred: options.deferred,
        error_message: options.error_message,
        response_id: options.response_id,
        timestamp: options
            .timestamp
            .unwrap_or_else(crate::ai::auth::resolve::now_millis),
        ..Default::default()
    }
}

fn blocks_to_text(blocks: &[crate::ai::types::BlockContent]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            crate::ai::types::BlockContent::Text(text) => text.text.clone(),
            crate::ai::types::BlockContent::Image(image) => {
                format!("[image:{}:{}]", image.mime_type, image.data.len())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_content_to_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(text) => text.text.clone(),
            AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
            AssistantContent::ToolCall(tool_call) => format!(
                "{}:{}",
                tool_call.name,
                serde_json::to_string(&tool_call.arguments).unwrap_or_default()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_to_text(message: &Message) -> String {
    match message {
        Message::User(message) => match &message.content {
            crate::ai::types::UserContent::Text(text) => text.clone(),
            crate::ai::types::UserContent::Blocks(blocks) => blocks_to_text(blocks),
        },
        Message::Assistant(message) => assistant_content_to_text(&message.content),
        Message::ToolResult(message) => {
            let mut parts = vec![message.tool_name.clone()];
            parts.push(blocks_to_text(&message.content));
            parts.join("\n")
        }
    }
}

fn serialize_context(context: &Context) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system_prompt) = &context.system_prompt {
        parts.push(format!("system:{system_prompt}"));
    }
    for message in &context.messages {
        parts.push(format!(
            "{}:{}",
            message_role(message),
            message_to_text(message)
        ));
    }
    if let Some(tools) = context.tools.as_ref()
        && !tools.is_empty()
    {
        parts.push(format!(
            "tools:{}",
            serde_json::to_string(&tools).unwrap_or_default()
        ));
    }
    parts.join("\n\n")
}

fn message_role(message: &Message) -> &'static str {
    match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

fn common_prefix_length(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let length = a.len().min(b.len());
    let mut index = 0;
    while index < length && a[index] == b[index] {
        index += 1;
    }
    index
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    prompt_cache: &mut HashMap<String, String>,
) -> AssistantMessage {
    let prompt_text = serialize_context(context);
    let prompt_tokens = estimate_tokens(&prompt_text);
    let output_tokens = estimate_tokens(&assistant_content_to_text(&message.content));
    let mut input = prompt_tokens;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    if let Some(session_id) = options.and_then(|options| options.base.session_id.clone())
        && options.and_then(|options| options.base.cache_retention)
            != Some(crate::ai::types::CacheRetention::None)
    {
        let previous_prompt = prompt_cache.get(&session_id).cloned();
        match previous_prompt {
            Some(previous_prompt) => {
                let cached_chars = common_prefix_length(&previous_prompt, &prompt_text);
                cache_read = estimate_tokens(&previous_prompt[..cached_chars]);
                cache_write = estimate_tokens(&prompt_text[cached_chars..]);
                input = prompt_tokens.saturating_sub(cache_read);
            }
            None => {
                cache_write = prompt_tokens;
            }
        }
        prompt_cache.insert(session_id, prompt_text);
    }

    message.usage = Usage {
        input,
        output: output_tokens,
        cache_read,
        cache_write,
        total_tokens: input + output_tokens + cache_read + cache_write,
        cost: UsageCost::default(),
        ..Default::default()
    };
    message
}

fn split_string_by_token_size(
    text: &str,
    min_token_size: usize,
    max_token_size: usize,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let token_size =
            min_token_size + (rand_below((max_token_size - min_token_size + 1) as u32) as usize);
        let char_size = (token_size * 4).max(1);
        let end = (index + char_size).min(bytes.len());
        chunks.push(String::from_utf8_lossy(&bytes[index..end]).to_string());
        index += char_size;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn rand_below(bound: u32) -> u32 {
    if bound == 0 {
        return 0;
    }
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    u32::from_le_bytes(bytes) % bound
}

fn clone_message(
    message: &AssistantMessage,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    let mut cloned = message.clone();
    cloned.api = api.to_string();
    cloned.provider = provider.to_string();
    cloned.model = model_id.to_string();
    cloned
}

fn create_deferred_message(model: &Model, handle: DeferredHandle) -> AssistantMessage {
    AssistantMessage {
        role: crate::ai::types::RoleAssistant,
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Deferred,
        deferred: Some(handle),
        timestamp: crate::ai::auth::resolve::now_millis(),
        ..Default::default()
    }
}

fn create_error_message(
    error: &str,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    AssistantMessage {
        role: crate::ai::types::RoleAssistant,
        content: Vec::new(),
        api: api.to_string(),
        provider: provider.to_string(),
        model: model_id.to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(error.to_string()),
        timestamp: crate::ai::auth::resolve::now_millis(),
        ..Default::default()
    }
}

fn create_aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    let mut aborted = partial.clone();
    aborted.stop_reason = StopReason::Aborted;
    aborted.error_message = Some("Request was aborted".to_string());
    aborted.timestamp = crate::ai::auth::resolve::now_millis();
    aborted
}

/// Streams a message as start/delta/end events, terminating the stream
/// according to the message stop reason. Port of `streamWithDeltas`.
async fn stream_with_deltas(
    stream: AssistantMessageEventStream,
    message: AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    signal: Option<&tokio_util::sync::CancellationToken>,
) {
    let mut partial = message.clone();
    partial.content = Vec::new();
    partial.stop_reason = StopReason::Pending;
    if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        let aborted = create_aborted_message(&partial);
        stream.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: aborted.clone(),
        });
        stream.end(Some(aborted));
        return;
    }

    stream.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    for index in 0..message.content.len() {
        if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            let aborted = create_aborted_message(&partial);
            stream.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error: aborted.clone(),
            });
            stream.end(Some(aborted));
            return;
        }

        let block = message.content[index].clone();

        match block {
            AssistantContent::Thinking(thinking) => {
                partial
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        ..Default::default()
                    }));
                stream.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in
                    split_string_by_token_size(&thinking.thinking, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                        let aborted = create_aborted_message(&partial);
                        stream.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    if let Some(AssistantContent::Thinking(slot)) = partial.content.get_mut(index) {
                        slot.thinking.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.thinking.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::Text(text) => {
                partial.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    ..Default::default()
                }));
                stream.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_string_by_token_size(&text.text, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                        let aborted = create_aborted_message(&partial);
                        stream.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    if let Some(AssistantContent::Text(slot)) = partial.content.get_mut(index) {
                        slot.text.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.text.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::ToolCall(tool_call) => {
                partial.content.push(AssistantContent::ToolCall(ToolCall {
                    content_type: Default::default(),
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: Default::default(),
                    ..Default::default()
                }));
                stream.push(AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                let arguments_text =
                    serde_json::to_string(&tool_call.arguments).unwrap_or_default();
                for chunk in
                    split_string_by_token_size(&arguments_text, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                        let aborted = create_aborted_message(&partial);
                        stream.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    stream.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                if let Some(AssistantContent::ToolCall(slot)) = partial.content.get_mut(index) {
                    slot.arguments = tool_call.arguments.clone();
                }
                stream.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call: tool_call.clone(),
                    partial: partial.clone(),
                });
            }
        }
    }

    if message.stop_reason == StopReason::Pending {
        let error = create_error_message(
            "Faux response ended without a stop reason",
            &partial.api,
            &partial.provider,
            &partial.model,
        );
        stream.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: error.clone(),
        });
        stream.end(Some(error));
        return;
    }
    if message.stop_reason == StopReason::Error || message.stop_reason == StopReason::Aborted {
        stream.push(AssistantMessageEvent::Error {
            reason: if message.stop_reason == StopReason::Aborted {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            },
            error: message.clone(),
        });
        stream.end(Some(message));
        return;
    }

    let reason = match message.stop_reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        _ => DoneReason::Stop,
    };
    stream.push(AssistantMessageEvent::Done {
        reason,
        message: message.clone(),
    });
    stream.end(Some(message));
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second {
        None => {}
        Some(rate) if rate <= 0.0 => {}
        Some(rate) => {
            let delay_ms = (estimate_tokens(chunk) as f64 / rate) * 1000.0;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
        }
    }
}

struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: Context,
    options: Option<SimpleStreamOptions>,
    model: Model,
    pending_fetches: u32,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

/// Port of the faux stream implementation behind `createFauxCore`.
pub struct FauxProviderStreams {
    api: String,
    provider: String,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    pub(crate) state: Arc<Mutex<FauxProviderState>>,
    pub(crate) pending_responses: Mutex<Vec<FauxResponseStep>>,
    prompt_cache: Arc<Mutex<HashMap<String, String>>>,
    deferred_responses: Arc<Mutex<HashMap<String, DeferredEntry>>>,
    deferred_options: Option<FauxDeferredOptions>,
}

impl ProviderStreams for FauxProviderStreams {
    fn stream(
        &self,
        request_model: &Model,
        context: &Context,
        stream_options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let simple = stream_options.map(|options| SimpleStreamOptions {
            base: options.clone(),
            ..Default::default()
        });
        self.stream_simple(request_model, context, simple.as_ref())
    }

    fn stream_simple(
        &self,
        request_model: &Model,
        context: &Context,
        stream_options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let outer = create_assistant_message_event_stream();
        let step = {
            let mut pending = self
                .pending_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.is_empty() {
                None
            } else {
                Some(pending.remove(0))
            }
        };
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.call_count += 1;
        }

        let request_model = request_model.clone();
        let context = context.clone();
        let stream_options = stream_options.cloned();
        let producer = outer.clone();
        let inner = InnerStreams {
            api: self.api.clone(),
            provider: self.provider.clone(),
            min_token_size: self.min_token_size,
            max_token_size: self.max_token_size,
            tokens_per_second: self.tokens_per_second,
            state: Arc::clone(&self.state),
            prompt_cache: Arc::clone(&self.prompt_cache),
            deferred_responses: Arc::clone(&self.deferred_responses),
            deferred_options: self.deferred_options,
        };
        tokio::spawn(async move {
            let outer = producer;
            let signal = stream_options
                .as_ref()
                .and_then(|options| options.base.base.signal.clone());
            if let Some(options) = &stream_options
                && let Some(on_response) = &options.base.base.on_response
            {
                on_response(
                    &crate::ai::types::ProviderResponse {
                        status: 200,
                        headers: Default::default(),
                    },
                    &request_model,
                )
                .await;
            }

            let step = match step {
                Some(step) => step,
                None => {
                    let mut message = create_error_message(
                        "No more faux responses queued",
                        &inner.api,
                        &inner.provider,
                        &request_model.id,
                    );
                    let mut cache = inner
                        .prompt_cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    message =
                        with_usage_estimate(message, &context, stream_options.as_ref(), &mut cache);
                    drop(cache);
                    outer.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: message.clone(),
                    });
                    outer.end(Some(message));
                    return;
                }
            };

            if stream_options
                .as_ref()
                .is_some_and(|options| options.deferred.is_some())
            {
                let handle = DeferredHandle {
                    provider: request_model.provider.clone(),
                    model_id: request_model.id.clone(),
                    api: request_model.api.clone(),
                    id: random_id("deferred"),
                    poll_after_ms: inner
                        .deferred_options
                        .and_then(|options| options.poll_after_ms),
                    ..Default::default()
                };
                let pending_fetches = inner
                    .deferred_options
                    .and_then(|options| options.pending_fetches)
                    .unwrap_or(0);
                inner
                    .deferred_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        handle.id.clone(),
                        DeferredEntry {
                            handle: handle.clone(),
                            step: step.clone(),
                            context: context.clone(),
                            options: stream_options.clone(),
                            model: request_model.clone(),
                            pending_fetches,
                            cancelled: false,
                            final_message: None,
                        },
                    );
                stream_with_deltas(
                    outer.clone(),
                    create_deferred_message(&request_model, handle),
                    inner.min_token_size,
                    inner.max_token_size,
                    inner.tokens_per_second,
                    signal.as_ref(),
                )
                .await;
                return;
            }

            let message = match inner.resolve_response(
                &step,
                &context,
                stream_options.as_ref(),
                &request_model,
            ) {
                Ok(message) => message,
                Err(error) => {
                    let message = create_error_message(
                        &error,
                        &inner.api,
                        &inner.provider,
                        &request_model.id,
                    );
                    outer.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: message.clone(),
                    });
                    outer.end(Some(message));
                    return;
                }
            };
            stream_with_deltas(
                outer.clone(),
                message,
                inner.min_token_size,
                inner.max_token_size,
                inner.tokens_per_second,
                signal.as_ref(),
            )
            .await;
        });

        outer
    }

    fn fetch_deferred(
        &self,
        request_model: &Model,
        handle: &DeferredHandle,
        fetch_options: Option<&crate::ai::types::DeferredFetchOptions>,
    ) -> Option<AssistantMessageEventStream> {
        let outer = create_assistant_message_event_stream();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.deferred_fetch_count += 1;
        }

        let request_model = request_model.clone();
        let handle = handle.clone();
        let fetch_signal = fetch_options.and_then(|options| options.base.signal.clone());
        let producer = outer.clone();
        let inner = InnerStreams {
            api: self.api.clone(),
            provider: self.provider.clone(),
            min_token_size: self.min_token_size,
            max_token_size: self.max_token_size,
            tokens_per_second: self.tokens_per_second,
            state: Arc::clone(&self.state),
            prompt_cache: Arc::clone(&self.prompt_cache),
            deferred_responses: Arc::clone(&self.deferred_responses),
            deferred_options: self.deferred_options,
        };
        tokio::spawn(async move {
            let result = run_deferred_fetch(
                &inner,
                &producer,
                &request_model,
                &handle,
                fetch_signal.as_ref(),
            )
            .await;
            if let Err(message) = result {
                let error =
                    create_error_message(&message, &inner.api, &inner.provider, &request_model.id);
                producer.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: error.clone(),
                });
                producer.end(Some(error));
            }
        });

        Some(outer)
    }

    fn cancel_deferred(
        &self,
        _request_model: &Model,
        handle: &DeferredHandle,
        _cancel_options: Option<&ProviderRequestOptions>,
    ) -> Option<BoxFuture<'static, Result<(), ModelsError>>> {
        let state = Arc::clone(&self.state);
        let deferred_responses = Arc::clone(&self.deferred_responses);
        let handle = handle.clone();
        Some(Box::pin(async move {
            {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.cancelled_deferred.push(handle.clone());
            }
            if let Some(entry) = deferred_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&handle.id)
            {
                entry.cancelled = true;
            }
            Ok(())
        }))
    }

    fn supports_deferred(&self) -> bool {
        true
    }

    fn supports_cancel_deferred(&self) -> bool {
        true
    }
}

/// Owned snapshot of the stream configuration shared into background tasks.
struct InnerStreams {
    api: String,
    provider: String,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    state: Arc<Mutex<FauxProviderState>>,
    prompt_cache: Arc<Mutex<HashMap<String, String>>>,
    deferred_responses: Arc<Mutex<HashMap<String, DeferredEntry>>>,
    deferred_options: Option<FauxDeferredOptions>,
}

impl InnerStreams {
    fn resolve_response(
        &self,
        step: &FauxResponseStep,
        context: &Context,
        stream_options: Option<&SimpleStreamOptions>,
        request_model: &Model,
    ) -> Result<AssistantMessage, String> {
        let resolved = match step {
            FauxResponseStep::Message(message) => *message.clone(),
            FauxResponseStep::Factory(factory) => factory(
                context,
                stream_options,
                &self.state.lock().unwrap(),
                request_model,
            )?,
        };
        let cloned = clone_message(&resolved, &self.api, &self.provider, &request_model.id);
        let mut cache = self
            .prompt_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(with_usage_estimate(
            cloned,
            context,
            stream_options,
            &mut cache,
        ))
    }
}

async fn run_deferred_fetch(
    inner: &InnerStreams,
    outer: &AssistantMessageEventStream,
    request_model: &Model,
    handle: &DeferredHandle,
    fetch_signal: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), String> {
    // Phase 1: inspect the entry under the lock without holding the guard
    // across any await.
    struct NeedsResolveData {
        step: FauxResponseStep,
        context: Context,
        model: Model,
        options: Option<SimpleStreamOptions>,
    }
    #[allow(clippy::large_enum_variant)]
    enum Outcome {
        Pending(Box<AssistantMessage>),
        NeedsResolve(NeedsResolveData),
        Ready(Box<AssistantMessage>),
    }
    let outcome = {
        let mut entries = inner
            .deferred_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = entries.get_mut(&handle.id) else {
            return Err(format!("Unknown faux deferred response: {}", handle.id));
        };
        if entry.handle.provider != handle.provider
            || entry.handle.model_id != handle.model_id
            || entry.handle.api != handle.api
        {
            return Err(format!("Unknown faux deferred response: {}", handle.id));
        }
        if entry.cancelled {
            return Err(format!(
                "Faux deferred response was cancelled: {}",
                handle.id
            ));
        }
        if entry.pending_fetches > 0 {
            entry.pending_fetches -= 1;
            Outcome::Pending(Box::new(create_deferred_message(
                request_model,
                entry.handle.clone(),
            )))
        } else if let Some(final_message) = entry.final_message.clone() {
            Outcome::Ready(Box::new(final_message))
        } else {
            Outcome::NeedsResolve(NeedsResolveData {
                step: entry.step.clone(),
                context: entry.context.clone(),
                model: entry.model.clone(),
                options: entry.options.clone(),
            })
        }
    };

    // Resolve a scripted response if one has not run yet. Locks stay scoped
    // to this block; no guard crosses the streaming await below.
    let message = match outcome {
        Outcome::Pending(deferred) => *deferred,
        Outcome::NeedsResolve(data) => {
            let NeedsResolveData {
                step,
                context,
                model,
                options,
            } = data;
            let final_message = inner
                .resolve_response(&step, &context, options.as_ref(), &model)
                .unwrap_or_else(|error| {
                    create_error_message(&error, &inner.api, &inner.provider, &model.id)
                });
            let mut entries = inner
                .deferred_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = entries.get_mut(&handle.id) {
                entry.final_message = Some(final_message.clone());
            }
            drop(entries);
            final_message
        }
        Outcome::Ready(final_message) => *final_message,
    };

    stream_with_deltas(
        outer.clone(),
        message,
        inner.min_token_size,
        inner.max_token_size,
        inner.tokens_per_second,
        fetch_signal,
    )
    .await;
    Ok(())
}

/// Port of `createFauxCore` + `fauxProvider`: builds the faux provider and
/// its handle for scripted responses.
pub fn faux_provider(options: RegisterFauxProviderOptions) -> FauxProviderHandle {
    let api = options
        .api
        .clone()
        .unwrap_or_else(|| random_id(DEFAULT_API));
    let provider = options
        .provider
        .clone()
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
    let min_token_size = options
        .token_size
        .and_then(|size| size.min)
        .unwrap_or(DEFAULT_MIN_TOKEN_SIZE)
        .max(1)
        .min(
            options
                .token_size
                .and_then(|size| size.max)
                .unwrap_or(DEFAULT_MAX_TOKEN_SIZE),
        );
    let max_token_size = options
        .token_size
        .and_then(|size| size.max)
        .unwrap_or(DEFAULT_MAX_TOKEN_SIZE)
        .max(min_token_size);

    let model_definitions = if options.models.is_empty() {
        vec![FauxModelDefinition {
            id: DEFAULT_MODEL_ID.to_string(),
            name: Some(DEFAULT_MODEL_NAME.to_string()),
            reasoning: Some(false),
            input: Some(vec![
                crate::ai::types::ModelInput::Text,
                crate::ai::types::ModelInput::Image,
            ]),
            cost: Some(crate::ai::types::ModelCost::default()),
            context_window: Some(128_000),
            max_tokens: Some(16_384),
        }]
    } else {
        options.models.clone()
    };
    let models: Vec<Model> = model_definitions
        .iter()
        .map(|definition| Model {
            id: definition.id.clone(),
            name: definition
                .name
                .clone()
                .unwrap_or_else(|| definition.id.clone()),
            api: api.clone(),
            provider: provider.clone(),
            base_url: DEFAULT_BASE_URL.to_string(),
            reasoning: definition.reasoning.unwrap_or(false),
            input: definition.input.clone().unwrap_or_else(|| {
                vec![
                    crate::ai::types::ModelInput::Text,
                    crate::ai::types::ModelInput::Image,
                ]
            }),
            cost: definition.cost.clone().unwrap_or_default(),
            context_window: definition.context_window.unwrap_or(128_000),
            max_tokens: definition.max_tokens.unwrap_or(16_384),
            ..Default::default()
        })
        .collect();

    let streams = Arc::new(FauxProviderStreams {
        api: api.clone(),
        provider: provider.clone(),
        min_token_size,
        max_token_size,
        tokens_per_second: options.tokens_per_second,
        state: Arc::new(Mutex::new(FauxProviderState::default())),
        pending_responses: Mutex::new(Vec::new()),
        prompt_cache: Arc::new(Mutex::new(HashMap::new())),
        deferred_responses: Arc::new(Mutex::new(HashMap::new())),
        deferred_options: options.deferred,
    });

    let auth = crate::ai::auth::types::ProviderAuth::api_key(Arc::new(FauxApiKeyAuth));
    let core_provider: Arc<BasicProvider> = Arc::new(BasicProvider::new(CreateProviderOptions {
        id: provider.clone(),
        organization_id: organization_for_provider(&provider).map(str::to_string),
        name: None,
        base_url: None,
        headers: None,
        auth,
        models: models.clone(),
        fetch_models: None,
        filter_models: None,
        api: crate::ai::models::ProviderApi::Single(
            Arc::clone(&streams) as Arc<dyn ProviderStreams>
        ),
    }));

    FauxProviderHandle {
        api,
        provider: core_provider,
        models,
        state: Arc::clone(&streams.state),
        streams,
    }
}

/// Port of `FauxProviderHandle`.
pub struct FauxProviderHandle {
    pub api: String,
    pub provider: Arc<dyn crate::ai::models::Provider>,
    pub models: Vec<Model>,
    pub state: Arc<Mutex<FauxProviderState>>,
    pub(crate) streams: Arc<FauxProviderStreams>,
}

impl FauxProviderHandle {
    /// The first faux model.
    pub fn get_model(&self) -> Model {
        self.models[0].clone()
    }

    /// Finds a model by id.
    pub fn get_model_by_id(&self, model_id: &str) -> Option<Model> {
        self.models
            .iter()
            .find(|model| model.id == model_id)
            .cloned()
    }

    /// Replaces the pending scripted responses.
    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        *self
            .streams
            .pending_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = responses;
    }

    /// Appends scripted responses.
    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.streams
            .pending_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(responses);
    }

    /// The number of responses still queued.
    pub fn get_pending_response_count(&self) -> usize {
        self.streams
            .pending_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// The faux provider's trivial api-key auth: always resolvable, no key.
struct FauxApiKeyAuth;

impl crate::ai::auth::types::ApiKeyAuth for FauxApiKeyAuth {
    fn name(&self) -> &str {
        "Faux"
    }

    fn resolve(
        &self,
        _input: crate::ai::auth::types::ApiKeyAuthInput,
    ) -> crate::ai::auth::types::AuthFuture<
        Result<
            Option<crate::ai::auth::types::AuthResult>,
            crate::ai::auth::types::AuthStorageError,
        >,
    > {
        Box::pin(async {
            Ok(Some(crate::ai::auth::types::AuthResult {
                auth: crate::ai::auth::types::ModelAuth::default(),
                env: None,
                source: None,
            }))
        })
    }
}
