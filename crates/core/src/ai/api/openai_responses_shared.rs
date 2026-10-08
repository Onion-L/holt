//! Port of `pi-core/ai/src/api/openai-responses-shared.ts`: the shared
//! Responses-API message conversion and stream processing used by
//! openai-responses, azure-openai-responses, and openai-codex-responses.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::{
    GrammarFormat, GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta,
    get_grammar_tool_input, get_json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::models::calculate_cost;
use crate::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Model, StopReason,
    TextContent, ThinkingContent, Tool, ToolCall, ToolCallArguments, Usage, UsageCost,
};
use crate::ai::utils::event_stream::AssistantMessageEventStream;
use crate::ai::utils::json_parse::parse_streaming_json;
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;
use crate::ai::utils::text::short_hash;

/// Port of `encodeTextSignatureV1`.
fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    let mut payload = json!({ "v": 1, "id": id });
    if let Some(phase) = phase {
        payload["phase"] = json!(phase);
    }
    serde_json::to_string(&payload).unwrap_or_default()
}

/// Port of `parseTextSignature`.
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v") == Some(&json!(1))
        && let Some(id) = parsed.get("id").and_then(Value::as_str)
    {
        let phase = parsed.get("phase").and_then(Value::as_str);
        if matches!(phase, Some("commentary") | Some("final_answer")) {
            return Some((id.to_string(), phase.map(str::to_string)));
        }
        return Some((id.to_string(), None));
    }
    Some((signature.to_string(), None))
}

/// Port of `convertToolResultOutput`.
fn convert_tool_result_output(model: &Model, content: &[crate::ai::types::BlockContent]) -> Value {
    let text_result = content
        .iter()
        .filter_map(|block| match block {
            crate::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&crate::ai::types::ImageContent> = content
        .iter()
        .filter_map(|block| match block {
            crate::ai::types::BlockContent::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    let has_text = !text_result.is_empty();

    if images.is_empty() || !model.input.contains(&crate::ai::types::ModelInput::Image) {
        let fallback = if has_text {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return Value::String(sanitize_surrogates(&fallback));
    }

    let mut output: Vec<Value> = Vec::new();
    if has_text {
        output.push(json!({"type": "input_text", "text": sanitize_surrogates(&text_result)}));
    }
    for image in images {
        output.push(json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
        }));
    }
    Value::Array(output)
}

/// Callback resolving the effective service tier.
pub type ResolveServiceTierFn =
    Box<dyn Fn(Option<&str>, Option<&str>) -> Option<String> + Send + Sync>;

/// Callback applying service-tier pricing to a usage record.
pub type ApplyServiceTierPricingFn = Box<dyn Fn(&mut Usage, Option<&str>) + Send + Sync>;

/// Port of `OpenAIResponsesStreamOptions`.
#[derive(Default)]
pub struct ResponsesStreamOptions {
    pub service_tier: Option<String>,
    pub grammar_tool_input_properties: Option<GrammarToolInputProperties>,
    /// Receives (response_service_tier, request_service_tier).
    pub resolve_service_tier: Option<ResolveServiceTierFn>,
    pub apply_service_tier_pricing: Option<ApplyServiceTierPricingFn>,
}

/// Grammar tool input properties keyed by tool name (shared type).
pub type GrammarToolInputProperties = std::collections::BTreeMap<String, String>;

/// Port of `ConvertResponsesMessagesOptions`.
#[derive(Default)]
pub struct ConvertResponsesMessagesOptions<'a> {
    pub include_system_prompt: Option<bool>,
    pub grammar_tool_input_properties: Option<&'a GrammarToolInputProperties>,
    pub deferred_tools: Option<&'a indexmap::IndexMap<String, Tool>>,
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    pub tool_options: Option<ConvertResponsesToolsOptions>,
}

/// The deferred-tools serialization modes for the Responses API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredToolsMode {
    AdditionalTools,
    ToolSearch,
}

/// Port of `ConvertResponsesToolsOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConvertResponsesToolsOptions {
    pub strict: Option<Option<bool>>,
    pub supports_strict_mode: Option<bool>,
    pub supports_openai_grammar_tools: Option<bool>,
    pub defer_loading: Option<bool>,
}

/// Port of `convertResponsesMessages`.
#[allow(clippy::too_many_lines)]
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &BTreeSet<String>,
    options: Option<&ConvertResponsesMessagesOptions<'_>>,
) -> Result<Vec<Value>, String> {
    let mut messages: Vec<Value> = Vec::new();
    let mut loaded_tool_names: BTreeSet<String> = BTreeSet::new();

    let normalize_id_part = |part: &str| -> String {
        let sanitized: String = part
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let normalized: String = sanitized.chars().take(64).collect();
        normalized.trim_end_matches('_').to_string()
    };

    let build_foreign_responses_item_id = |item_id: &str| -> String {
        let normalized = format!("fc_{}", short_hash(item_id));
        normalized.chars().take(64).collect()
    };

    let normalize_tool_call_id =
        |id: &str, source: &crate::ai::types::AssistantMessage| -> String {
            if !allowed_tool_call_providers.contains(&model.provider) {
                return normalize_id_part(id);
            }
            if !id.contains('|') {
                return normalize_id_part(id);
            }
            let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
            let normalized_call_id = normalize_id_part(call_id);
            let is_foreign_tool_call = source.provider != model.provider || source.api != model.api;
            let mut normalized_item_id = if is_foreign_tool_call {
                build_foreign_responses_item_id(item_id)
            } else {
                normalize_id_part(item_id)
            };
            // OpenAI Responses API requires item id to start with "fc".
            if !normalized_item_id.starts_with("fc_") {
                normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
            }
            format!("{normalized_call_id}|{normalized_item_id}")
        };

    let transformed_messages =
        transform_messages(&context.messages, model, Some(&normalize_tool_call_id));

    let include_system_prompt = options
        .and_then(|options| options.include_system_prompt)
        .unwrap_or(true);
    if include_system_prompt && let Some(system_prompt) = &context.system_prompt {
        let supports_developer_role = model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_developer_role);
        let role = if model.reasoning && supports_developer_role != Some(false) {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({
            "role": role,
            "content": sanitize_surrogates(system_prompt),
        }));
    }

    let mut message_index = 0usize;
    for message in &transformed_messages {
        match message {
            crate::ai::types::Message::User(user) => match &user.content {
                crate::ai::types::UserContent::Text(text) => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{"type": "input_text", "text": sanitize_surrogates(text)}],
                    }));
                }
                crate::ai::types::UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            crate::ai::types::BlockContent::Text(text) => json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            crate::ai::types::BlockContent::Image(image) => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    messages.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            },
            crate::ai::types::Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let is_same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let is_same_model = is_same_provider_and_api && assistant.model == model.id;
                let is_different_model = is_same_provider_and_api && assistant.model != model.id;
                let mut text_block_index = 0usize;

                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = &thinking.thinking_signature {
                                let reasoning_item: Value =
                                    serde_json::from_str(signature).unwrap_or(Value::Null);
                                output.push(reasoning_item);
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed_signature =
                                parse_text_signature(text.text_signature.as_deref());
                            let fallback_message_id = if text_block_index == 0 {
                                format!("msg_pi_{message_index}")
                            } else {
                                format!("msg_pi_{message_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            // OpenAI requires id to be max 64 characters.
                            let msg_id = match parsed_signature.as_ref().map(|(id, _)| id) {
                                Some(id) if id.chars().count() > 64 => {
                                    format!("msg_{}", short_hash(id))
                                }
                                Some(id) => id.clone(),
                                None => fallback_message_id,
                            };
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": sanitize_surrogates(&text.text),
                                    "annotations": [],
                                }],
                                "status": "completed",
                                "id": msg_id,
                            });
                            if let Some(phase) = parsed_signature
                                .as_ref()
                                .and_then(|(_, phase)| phase.as_deref())
                            {
                                item["phase"] = json!(phase);
                            }
                            output.push(item);
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let (call_id, item_id_raw) = tool_call
                                .id
                                .split_once('|')
                                .unwrap_or((tool_call.id.as_str(), ""));
                            let custom_input_property = options
                                .and_then(|options| options.grammar_tool_input_properties)
                                .and_then(|properties| properties.get(&tool_call.name))
                                .cloned();
                            let mut item_id: Option<String> = Some(item_id_raw.to_string());

                            // For different-model messages, drop the id to
                            // avoid pairing validation; non-fc_* ids are also
                            // dropped for function_call replay.
                            if (is_different_model
                                && item_id.as_ref().is_some_and(|id| id.starts_with("fc_")))
                                || (custom_input_property.is_none()
                                    && !item_id.as_ref().is_some_and(|id| id.starts_with("fc_")))
                            {
                                item_id = None;
                            }

                            let can_replay_namespace = is_same_model
                                || options
                                    .and_then(|options| options.deferred_tools)
                                    .is_some_and(|tools| tools.contains_key(&tool_call.name));

                            if let Some(custom_input_property) = custom_input_property {
                                let mut item = json!({
                                    "type": "custom_tool_call",
                                    "id": item_id,
                                    "call_id": call_id,
                                    "name": tool_call.name,
                                    "input": sanitize_surrogates(&get_grammar_tool_input(
                                        &tool_call.name,
                                        &tool_call.arguments,
                                        &custom_input_property,
                                    )?),
                                });
                                if can_replay_namespace
                                    && let Some(namespace) = &tool_call.namespace
                                {
                                    item["namespace"] = json!(namespace);
                                }
                                output.push(item);
                            } else {
                                let mut item = json!({
                                    "type": "function_call",
                                    "id": item_id,
                                    "call_id": call_id,
                                    "name": tool_call.name,
                                    "arguments": serde_json::to_string(&tool_call.arguments).unwrap_or_default(),
                                });
                                if can_replay_namespace
                                    && let Some(namespace) = &tool_call.namespace
                                {
                                    item["namespace"] = json!(namespace);
                                }
                                output.push(item);
                            }
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            crate::ai::types::Message::ToolResult(result) => {
                let (call_id, _) = result
                    .tool_call_id
                    .split_once('|')
                    .unwrap_or((&result.tool_call_id, ""));
                let output = convert_tool_result_output(model, &result.content);

                let is_grammar = options
                    .and_then(|options| options.grammar_tool_input_properties)
                    .is_some_and(|properties| properties.contains_key(&result.tool_name));
                if is_grammar {
                    messages.push(json!({
                        "type": "custom_tool_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                } else {
                    messages.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                }

                let mut deferred_tools: Vec<Tool> = Vec::new();
                for name in result.added_tool_names.iter().flatten() {
                    let Some(tool) = options
                        .and_then(|options| options.deferred_tools)
                        .and_then(|tools| tools.get(name))
                    else {
                        continue;
                    };
                    if loaded_tool_names.contains(name) {
                        continue;
                    }
                    loaded_tool_names.insert(name.clone());
                    deferred_tools.push(tool.clone());
                }
                if !deferred_tools.is_empty()
                    && options.and_then(|options| options.deferred_tools_mode)
                        == Some(DeferredToolsMode::AdditionalTools)
                {
                    let tool_options = options
                        .and_then(|options| options.tool_options)
                        .unwrap_or_default();
                    messages.push(json!({
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": convert_responses_tools(&deferred_tools, Some(&tool_options))?,
                    }));
                } else if !deferred_tools.is_empty()
                    && options.and_then(|options| options.deferred_tools_mode)
                        == Some(DeferredToolsMode::ToolSearch)
                {
                    let names: Vec<String> = deferred_tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect();
                    let search_call_id = format!(
                        "pi_tool_load_{}",
                        short_hash(&format!("{}:{}", result.tool_call_id, names.join(",")))
                    );
                    messages.push(json!({
                        "type": "tool_search_call",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "arguments": {"query": names.join(" "), "limit": names.len()},
                    }));
                    let mut tool_options = options
                        .and_then(|options| options.tool_options)
                        .unwrap_or_default();
                    tool_options.defer_loading = Some(true);
                    messages.push(json!({
                        "type": "tool_search_output",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "tools": convert_responses_tools(&deferred_tools, Some(&tool_options))?,
                    }));
                }
            }
        }
        message_index += 1;
    }

    Ok(messages)
}

/// Port of `convertResponsesTools`.
pub fn convert_responses_tools(
    tools: &[Tool],
    options: Option<&ConvertResponsesToolsOptions>,
) -> Result<Vec<Value>, String> {
    let options = options.cloned().unwrap_or_default();
    // `options.strict === undefined ? false : options.strict`: a `null`
    // default must serialize as `strict: null` (Codex sets it explicitly).
    let default_strict = match options.strict {
        Some(value) => value,
        None => Some(false),
    };
    let supports_strict_mode = options.supports_strict_mode.unwrap_or(true);
    let supports_openai_grammar_tools = options.supports_openai_grammar_tools.unwrap_or(false);

    tools
        .iter()
        .map(|tool| {
            let grammar =
                resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?;
            if let Some(grammar) = grammar {
                let syntax = if grammar.format == GrammarFormat::Lark {
                    "lark"
                } else {
                    "regex"
                };
                let mut converted = json!({
                    "type": "custom",
                    "name": tool.name,
                    "description": tool.description,
                    "format": {
                        "type": "grammar",
                        "syntax": syntax,
                        "definition": grammar.definition,
                    },
                });
                if options.defer_loading == Some(true) {
                    converted["defer_loading"] = json!(true);
                }
                return Ok(converted);
            }

            let constrained_strict =
                resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
            let strict = constrained_strict.or(default_strict);
            let mut function_tool = Map::new();
            function_tool.insert("type".to_string(), json!("function"));
            function_tool.insert("name".to_string(), json!(tool.name));
            function_tool.insert("description".to_string(), json!(tool.description));
            function_tool.insert(
                "parameters".to_string(),
                get_json_schema_tool_parameters(tool, strict == Some(true))?,
            );
            if options.defer_loading == Some(true) {
                function_tool.insert("defer_loading".to_string(), json!(true));
            }
            if supports_strict_mode {
                function_tool.insert("strict".to_string(), json!(strict));
            }
            Ok(Value::Object(function_tool))
        })
        .collect()
}

/// A streaming output slot. Port of `ResponsesOutputSlot`.
#[derive(Clone, Debug)]
enum ResponsesOutputSlot {
    Thinking {
        /// Unused slot data kept for the TS shape parity.
        #[allow(dead_code)]
        thinking_signature: Option<String>,
        position: usize,
    },
    Text {
        position: usize,
    },
    ToolCall {
        scratch: ResponsesToolCallScratch,
        position: usize,
    },
}

#[derive(Clone, Debug, Default)]
struct ResponsesToolCallScratch {
    partial_json: Option<String>,
    custom_input: Option<(String, GrammarToolInputJsonBuffer)>,
}

fn encode_reasoning_item(item: &Value) -> String {
    serde_json::to_string(item).unwrap_or_default()
}

/// Port of `mapStopReason` for response status.
fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> Result<(StopReason, Option<String>), String> {
    let Some(status) = status else {
        return Ok((StopReason::Stop, None));
    };
    match status {
        "completed" => Ok((StopReason::Stop, None)),
        "incomplete" => {
            if incomplete_reason == Some("max_output_tokens") {
                Ok((StopReason::Length, None))
            } else {
                Ok((
                    StopReason::Error,
                    Some(
                        incomplete_reason
                            .map(|reason| format!("Response incomplete: {reason}"))
                            .unwrap_or_else(|| {
                                "Response incomplete without a provider reason".to_string()
                            }),
                    ),
                ))
            }
        }
        "failed" | "cancelled" => Ok((StopReason::Error, None)),
        // These two are wonky ...
        "in_progress" | "queued" => Ok((StopReason::Stop, None)),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

/// Port of `processResponsesStream`: consumes raw Responses SSE event values.
/// The TypeScript original iterates an async event iterable; the Rust port
/// takes a stream of `Result` items so provider adapters can surface mapping
/// errors (the TS generator `throw` path) mid-stream.
#[allow(clippy::too_many_lines)]
pub async fn process_responses_stream<S>(
    events: S,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
    model: &Model,
    options: Option<&ResponsesStreamOptions>,
) -> Result<(), String>
where
    S: futures::Stream<Item = Result<Value, String>>,
{
    #[allow(unused_assignments)]
    let mut saw_terminal_response_event = false;
    let mut output_slots: BTreeMap<u64, ResponsesOutputSlot> = BTreeMap::new();
    let mut reasoning_blocks_by_id: BTreeMap<String, ThinkingContent> = BTreeMap::new();

    let grammar_properties = options
        .and_then(|options| options.grammar_tool_input_properties.clone())
        .unwrap_or_default();

    let mut events = std::pin::pin!(events);
    while let Some(event) = futures::StreamExt::next(&mut events).await {
        let event = event?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match event_type.as_str() {
            "response.created" => {
                output.response_id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "response.output_item.added" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                create_slot(
                    &mut output_slots,
                    output_index,
                    &item,
                    output,
                    stream,
                    &grammar_properties,
                );
            }
            "response.reasoning_summary_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(position) =
                    get_slot_position(&output_slots, output_index, SlotKind::Thinking)
                else {
                    continue;
                };
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                    block.thinking.push_str(delta);
                }
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: delta.to_string(),
                    partial: output.clone(),
                });
            }
            "response.reasoning_summary_part.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(position) =
                    get_slot_position(&output_slots, output_index, SlotKind::Thinking)
                else {
                    continue;
                };
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                    block.thinking.push_str("\n\n");
                }
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: "\n\n".to_string(),
                    partial: output.clone(),
                });
            }
            "response.reasoning_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(position) =
                    get_slot_position(&output_slots, output_index, SlotKind::Thinking)
                else {
                    continue;
                };
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(position) {
                    block.thinking.push_str(delta);
                }
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: delta.to_string(),
                    partial: output.clone(),
                });
            }
            "response.output_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(position) = get_slot_position(&output_slots, output_index, SlotKind::Text)
                else {
                    continue;
                };
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Text(block)) = output.content.get_mut(position) {
                    block.text.push_str(delta);
                }
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta: delta.to_string(),
                    partial: output.clone(),
                });
            }
            "response.refusal.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some(position) = get_slot_position(&output_slots, output_index, SlotKind::Text)
                else {
                    continue;
                };
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(AssistantContent::Text(block)) = output.content.get_mut(position) {
                    block.text.push_str(delta);
                }
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta: delta.to_string(),
                    partial: output.clone(),
                });
            }
            "response.function_call_arguments.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some((position, has_partial)) =
                    get_slot_tool_call_partial(&output_slots, output_index)
                else {
                    continue;
                };
                if !has_partial {
                    continue;
                }
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(state) = output_slots.get_mut(&output_index)
                    && let ResponsesOutputSlot::ToolCall { scratch, .. } = state
                    && let Some(partial_json) = &mut scratch.partial_json
                {
                    partial_json.push_str(&delta);
                    let arguments = parse_streaming_json(Some(partial_json));
                    if let Some(AssistantContent::ToolCall(block)) =
                        output.content.get_mut(position)
                    {
                        block.arguments = arguments.as_object().cloned().unwrap_or_default();
                    }
                }
                stream.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: position,
                    delta,
                    partial: output.clone(),
                });
            }
            "response.function_call_arguments.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let Some((position, has_partial)) =
                    get_slot_tool_call_partial(&output_slots, output_index)
                else {
                    continue;
                };
                if !has_partial {
                    continue;
                }
                let arguments_text = event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let previous_partial_json = output_slots
                    .get(&output_index)
                    .and_then(|slot| match slot {
                        ResponsesOutputSlot::ToolCall { scratch, .. } => {
                            scratch.partial_json.clone()
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(state) = output_slots.get_mut(&output_index)
                    && let ResponsesOutputSlot::ToolCall { scratch, .. } = state
                {
                    scratch.partial_json = Some(arguments_text.clone());
                }
                let parsed = parse_streaming_json(Some(&arguments_text));
                if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
                    block.arguments = parsed.as_object().cloned().unwrap_or_default();
                }
                if arguments_text.starts_with(&previous_partial_json) {
                    let delta = &arguments_text[previous_partial_json.len()..];
                    if !delta.is_empty() {
                        stream.push(AssistantMessageEvent::ToolcallDelta {
                            content_index: position,
                            delta: delta.to_string(),
                            partial: output.clone(),
                        });
                    }
                }
            }
            "response.custom_tool_call_input.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if !slot_has_custom_input(&output_slots, output_index) {
                    continue;
                }
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let current = custom_tool_call_input(output, &output_slots, output_index);
                let next_input = format!("{current}{delta}");
                let push = append_custom_tool_call_input(
                    output,
                    &mut output_slots,
                    output_index,
                    &next_input,
                    false,
                );
                if let Some(delta) = push {
                    let position = slot_position(&output_slots, output_index);
                    stream.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: position,
                        delta,
                        partial: output.clone(),
                    });
                }
            }
            "response.custom_tool_call_input.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if !slot_has_custom_input(&output_slots, output_index) {
                    continue;
                }
                let input = event
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let push = append_custom_tool_call_input(
                    output,
                    &mut output_slots,
                    output_index,
                    &input,
                    true,
                );
                if let Some(delta) = push {
                    let position = slot_position(&output_slots, output_index);
                    stream.push(AssistantMessageEvent::ToolcallDelta {
                        content_index: position,
                        delta,
                        partial: output.clone(),
                    });
                }
            }
            "response.output_item.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("phase").and_then(Value::as_str) == Some("final_answer")
                {
                    output.stop_reason = StopReason::Stop;
                }
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                match item_type.as_str() {
                    "reasoning" => {
                        if get_slot_position(&output_slots, output_index, SlotKind::Thinking)
                            .is_none()
                        {
                            create_slot(
                                &mut output_slots,
                                output_index,
                                &item,
                                output,
                                stream,
                                &grammar_properties,
                            );
                        }
                        let position =
                            get_slot_position(&output_slots, output_index, SlotKind::Thinking);
                        let Some(position) = position else { continue };
                        let summary_text = item
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        let content_text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                        if let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(position)
                        {
                            block.thinking = if !summary_text.is_empty() {
                                summary_text
                            } else if !content_text.is_empty() {
                                content_text
                            } else {
                                block.thinking.clone()
                            };
                            block.thinking_signature = Some(encode_reasoning_item(&item));
                            if let Some(id) = item.get("id").and_then(Value::as_str) {
                                reasoning_blocks_by_id.insert(id.to_string(), block.clone());
                            }
                        }
                        stream.push(AssistantMessageEvent::ThinkingEnd {
                            content_index: position,
                            content: match output.content.get(position) {
                                Some(AssistantContent::Thinking(block)) => block.thinking.clone(),
                                _ => String::new(),
                            },
                            partial: output.clone(),
                        });
                        output_slots.remove(&output_index);
                    }
                    "message" => {
                        if get_slot_position(&output_slots, output_index, SlotKind::Text).is_none()
                        {
                            create_slot(
                                &mut output_slots,
                                output_index,
                                &item,
                                output,
                                stream,
                                &grammar_properties,
                            );
                        }
                        let Some(position) =
                            get_slot_position(&output_slots, output_index, SlotKind::Text)
                        else {
                            continue;
                        };
                        let text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| {
                                        part.get("text")
                                            .and_then(Value::as_str)
                                            .or_else(|| part.get("refusal").and_then(Value::as_str))
                                    })
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                        let phase = item.get("phase").and_then(Value::as_str);
                        if let Some(AssistantContent::Text(block)) =
                            output.content.get_mut(position)
                        {
                            block.text = text.clone();
                            block.text_signature = Some(encode_text_signature_v1(item_id, phase));
                        }
                        stream.push(AssistantMessageEvent::TextEnd {
                            content_index: position,
                            content: text,
                            partial: output.clone(),
                        });
                        output_slots.remove(&output_index);
                    }
                    "function_call" => {
                        let has_partial = matches!(
                            output_slots.get(&output_index),
                            Some(ResponsesOutputSlot::ToolCall {
                                scratch: ResponsesToolCallScratch {
                                    partial_json: Some(_),
                                    ..
                                },
                                ..
                            })
                        );
                        if has_partial {
                            let position = slot_position(&output_slots, output_index);
                            let item_arguments = item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let partial_json = output_slots
                                .get(&output_index)
                                .and_then(|slot| match slot {
                                    ResponsesOutputSlot::ToolCall { scratch, .. } => {
                                        scratch.partial_json.clone()
                                    }
                                    _ => None,
                                })
                                .unwrap_or_default();
                            let source = if !item_arguments.is_empty() {
                                item_arguments
                            } else if !partial_json.is_empty() {
                                &partial_json
                            } else {
                                "{}"
                            };
                            let parsed = parse_streaming_json(Some(source));
                            if let Some(AssistantContent::ToolCall(block)) =
                                output.content.get_mut(position)
                            {
                                block.arguments = parsed.as_object().cloned().unwrap_or_default();
                                if let Some(namespace) =
                                    item.get("namespace").and_then(Value::as_str)
                                {
                                    block.namespace = Some(namespace.to_string());
                                }
                            }
                            if let Some(state) = output_slots.get_mut(&output_index)
                                && let ResponsesOutputSlot::ToolCall { scratch, .. } = state
                            {
                                scratch.partial_json = None;
                            }
                            let tool_call = match output.content.get(position) {
                                Some(AssistantContent::ToolCall(tool_call)) => tool_call.clone(),
                                _ => continue,
                            };
                            stream.push(AssistantMessageEvent::ToolcallEnd {
                                content_index: position,
                                tool_call,
                                partial: output.clone(),
                            });
                            output_slots.remove(&output_index);
                        }
                    }
                    "custom_tool_call" => {
                        let has_custom = matches!(
                            output_slots.get(&output_index),
                            Some(ResponsesOutputSlot::ToolCall {
                                scratch: ResponsesToolCallScratch {
                                    custom_input: Some(_),
                                    ..
                                },
                                ..
                            })
                        );
                        if has_custom {
                            let item_input = item
                                .get("input")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let current =
                                custom_tool_call_input(output, &output_slots, output_index);
                            let next_input = if item_input.is_empty() {
                                current
                            } else {
                                item_input
                            };
                            let push = append_custom_tool_call_input(
                                output,
                                &mut output_slots,
                                output_index,
                                &next_input,
                                true,
                            );
                            if let Some(delta) = push {
                                let position = slot_position(&output_slots, output_index);
                                stream.push(AssistantMessageEvent::ToolcallDelta {
                                    content_index: position,
                                    delta,
                                    partial: output.clone(),
                                });
                            }
                            let position = slot_position(&output_slots, output_index);
                            if let Some(namespace) = item.get("namespace").and_then(Value::as_str)
                                && let Some(AssistantContent::ToolCall(block)) =
                                    output.content.get_mut(position)
                            {
                                block.namespace = Some(namespace.to_string());
                            }
                            if let Some(state) = output_slots.get_mut(&output_index)
                                && let ResponsesOutputSlot::ToolCall { scratch, .. } = state
                            {
                                scratch.custom_input = None;
                            }
                            let tool_call = match output.content.get(position) {
                                Some(AssistantContent::ToolCall(tool_call)) => tool_call.clone(),
                                _ => continue,
                            };
                            stream.push(AssistantMessageEvent::ToolcallEnd {
                                content_index: position,
                                tool_call,
                                partial: output.clone(),
                            });
                            output_slots.remove(&output_index);
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                finalize_response(
                    event.get("response").cloned().unwrap_or(Value::Null),
                    output,
                    model,
                    options,
                    &reasoning_blocks_by_id,
                    &mut saw_terminal_response_event,
                )?;
            }
            "error" => {
                let code = event
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown error");
                return Err(format!("Error Code {code}: {message}"));
            }
            "response.failed" => {
                // Terminal: the error return below prevents the "ended before
                // a terminal response event" failure, matching the TS flag.
                #[allow(unused_assignments)]
                {
                    saw_terminal_response_event = true;
                }
                output.raw_stop_reason = event
                    .pointer("/response/status")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let error = event.pointer("/response/error");
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str);
                let message = match error {
                    Some(error) => format!(
                        "{}: {}",
                        error
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no message")
                    ),
                    None => match reason {
                        Some(reason) => format!("incomplete: {reason}"),
                        None => "Unknown error (no error details in response)".to_string(),
                    },
                };
                return Err(message);
            }
            _ => {}
        }
    }
    if !saw_terminal_response_event {
        return Err("OpenAI Responses stream ended before a terminal response event".to_string());
    }
    Ok(())
}

fn custom_tool_call_input(
    output: &AssistantMessage,
    output_slots: &BTreeMap<u64, ResponsesOutputSlot>,
    output_index: u64,
) -> String {
    let property = output_slots.get(&output_index).and_then(|slot| match slot {
        ResponsesOutputSlot::ToolCall { scratch, .. } => scratch
            .custom_input
            .as_ref()
            .map(|(property, _)| property.clone()),
        _ => None,
    });
    let Some(property) = property else {
        return String::new();
    };
    output_slots
        .get(&output_index)
        .and_then(|slot| match slot {
            ResponsesOutputSlot::ToolCall { position, .. } => Some(*position),
            _ => None,
        })
        .and_then(|position| output.content.get(position))
        .and_then(|block| match block {
            AssistantContent::ToolCall(tool_call) => tool_call.arguments.get(&property),
            _ => None,
        })
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn append_custom_tool_call_input(
    output: &mut AssistantMessage,
    output_slots: &mut BTreeMap<u64, ResponsesOutputSlot>,
    output_index: u64,
    next_input: &str,
    close: bool,
) -> Option<String> {
    let position = output_slots
        .get(&output_index)
        .and_then(|slot| match slot {
            ResponsesOutputSlot::ToolCall { position, .. } => Some(*position),
            _ => None,
        })?;
    let state = output_slots.get_mut(&output_index)?;
    let ResponsesOutputSlot::ToolCall { scratch, .. } = state else {
        return None;
    };
    let (property, delta) = {
        let (property, buffer) = scratch.custom_input.as_mut()?;
        let delta =
            append_grammar_tool_input_json_delta(buffer, property, next_input, close).ok()??;
        (property.clone(), delta)
    };
    if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(position) {
        block.arguments = [(property, Value::String(next_input.to_string()))]
            .into_iter()
            .collect();
    }
    Some(delta)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    #[allow(dead_code)]
    ToolCall,
}

fn slot_matches(slot: &ResponsesOutputSlot, kind: SlotKind) -> bool {
    matches!(
        (slot, kind),
        (ResponsesOutputSlot::Thinking { .. }, SlotKind::Thinking)
            | (ResponsesOutputSlot::Text { .. }, SlotKind::Text)
            | (ResponsesOutputSlot::ToolCall { .. }, SlotKind::ToolCall)
    )
}

fn get_slot_position(
    slots: &BTreeMap<u64, ResponsesOutputSlot>,
    index: u64,
    kind: SlotKind,
) -> Option<usize> {
    let slot = slots.get(&index)?;
    slot_matches(slot, kind).then_some(match slot {
        ResponsesOutputSlot::Thinking { position, .. }
        | ResponsesOutputSlot::Text { position }
        | ResponsesOutputSlot::ToolCall { position, .. } => *position,
    })
}

fn get_slot_tool_call_partial(
    slots: &BTreeMap<u64, ResponsesOutputSlot>,
    index: u64,
) -> Option<(usize, bool)> {
    let slot = slots.get(&index)?;
    if let ResponsesOutputSlot::ToolCall { scratch, position } = slot {
        return Some((*position, scratch.partial_json.is_some()));
    }
    None
}

fn slot_position(slots: &BTreeMap<u64, ResponsesOutputSlot>, index: u64) -> usize {
    slots
        .get(&index)
        .map(|slot| match slot {
            ResponsesOutputSlot::Thinking { position, .. }
            | ResponsesOutputSlot::Text { position }
            | ResponsesOutputSlot::ToolCall { position, .. } => *position,
        })
        .unwrap_or_default()
}

fn slot_has_custom_input(slots: &BTreeMap<u64, ResponsesOutputSlot>, index: u64) -> bool {
    matches!(
        slots.get(&index),
        Some(ResponsesOutputSlot::ToolCall {
            scratch: ResponsesToolCallScratch {
                custom_input: Some(_),
                ..
            },
            ..
        })
    )
}

#[allow(clippy::too_many_lines)]
fn create_slot(
    slots: &mut BTreeMap<u64, ResponsesOutputSlot>,
    output_index: u64,
    item: &Value,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
    grammar_properties: &BTreeMap<String, String>,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if item_type == "message" && item.get("phase").and_then(Value::as_str) == Some("final_answer") {
        output.stop_reason = StopReason::Stop;
    }
    match item_type {
        "reasoning" => {
            output
                .content
                .push(AssistantContent::Thinking(ThinkingContent::default()));
            let position = output.content.len() - 1;
            slots.insert(
                output_index,
                ResponsesOutputSlot::Thinking {
                    thinking_signature: None,
                    position,
                },
            );
            stream.push(AssistantMessageEvent::ThinkingStart {
                content_index: position,
                partial: output.clone(),
            });
        }
        "message" => {
            if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                output.stop_reason = StopReason::Stop;
            }
            output
                .content
                .push(AssistantContent::Text(TextContent::default()));
            let position = output.content.len() - 1;
            slots.insert(output_index, ResponsesOutputSlot::Text { position });
            stream.push(AssistantMessageEvent::TextStart {
                content_index: position,
                partial: output.clone(),
            });
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let mut block = ToolCall {
                content_type: Default::default(),
                id: format!("{call_id}|{item_id}"),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: ToolCallArguments::new(),
                ..Default::default()
            };
            if let Some(namespace) = item.get("namespace").and_then(Value::as_str) {
                block.namespace = Some(namespace.to_string());
            }
            let partial_json = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = parse_streaming_json(Some(&partial_json));
            block.arguments = arguments.as_object().cloned().unwrap_or_default();
            output.content.push(AssistantContent::ToolCall(block));
            let position = output.content.len() - 1;
            slots.insert(
                output_index,
                ResponsesOutputSlot::ToolCall {
                    scratch: ResponsesToolCallScratch {
                        partial_json: Some(partial_json),
                        custom_input: None,
                    },
                    position,
                },
            );
            stream.push(AssistantMessageEvent::ToolcallStart {
                content_index: position,
                partial: output.clone(),
            });
        }
        "custom_tool_call" => {
            let input_property = grammar_properties
                .get(item.get("name").and_then(Value::as_str).unwrap_or_default())
                .cloned()
                .unwrap_or_else(|| "input".to_string());
            let input = item
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let mut block = ToolCall {
                content_type: Default::default(),
                id: format!("{call_id}|{item_id}"),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: [(input_property.clone(), Value::String(input))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            if let Some(namespace) = item.get("namespace").and_then(Value::as_str) {
                block.namespace = Some(namespace.to_string());
            }
            output.content.push(AssistantContent::ToolCall(block));
            let position = output.content.len() - 1;
            slots.insert(
                output_index,
                ResponsesOutputSlot::ToolCall {
                    scratch: ResponsesToolCallScratch {
                        partial_json: None,
                        custom_input: Some((input_property, GrammarToolInputJsonBuffer::default())),
                    },
                    position,
                },
            );
            stream.push(AssistantMessageEvent::ToolcallStart {
                content_index: position,
                partial: output.clone(),
            });
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_response(
    response: Value,
    output: &mut AssistantMessage,
    model: &Model,
    options: Option<&ResponsesStreamOptions>,
    reasoning_blocks_by_id: &BTreeMap<String, ThinkingContent>,
    saw_terminal_response_event: &mut bool,
) -> Result<(), String> {
    *saw_terminal_response_event = true;
    // Azure OpenAI can omit reasoning.encrypted_content from
    // response.output_item.done and provide it only in
    // response.completed.response.output; backfill the persisted signature so
    // store:false multi-turn replay stays stateless.
    if let Some(response_output) = response.get("output").and_then(Value::as_array) {
        for item in response_output {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(encrypted_content) = item.get("encrypted_content").and_then(Value::as_str)
            else {
                continue;
            };
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(block) = reasoning_blocks_by_id.get(id) else {
                continue;
            };
            let Some(signature) = &block.thinking_signature else {
                continue;
            };
            let Ok(mut stored_item) = serde_json::from_str::<Value>(signature) else {
                continue;
            };
            if stored_item.get("encrypted_content").is_some() {
                continue;
            }
            stored_item["encrypted_content"] = json!(encrypted_content);
            let updated = serde_json::to_string(&stored_item).unwrap_or_default();
            // Apply through output.content: the slot position is found by
            // signature identity.
            for block in output.content.iter_mut() {
                if let AssistantContent::Thinking(thinking) = block
                    && thinking.thinking_signature.as_deref() == Some(signature.as_str())
                {
                    thinking.thinking_signature = Some(updated.clone());
                }
            }
        }
    }
    if let Some(id) = response.get("id").and_then(Value::as_str) {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
        let get_u64 = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let cached_tokens = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_write_tokens = usage
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        output.usage = Usage {
            // OpenAI includes cached and cache-write tokens in input_tokens.
            input: get_u64("input_tokens")
                .saturating_sub(cached_tokens)
                .saturating_sub(cache_write_tokens),
            output: get_u64("output_tokens"),
            cache_read: cached_tokens,
            cache_write: cache_write_tokens,
            reasoning: Some(
                usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            total_tokens: get_u64("total_tokens"),
            cost: UsageCost::default(),
            ..Default::default()
        };
    }
    calculate_cost(model, &mut output.usage);
    if let Some(options) = options
        && let Some(apply_pricing) = &options.apply_service_tier_pricing
    {
        let response_tier = response.get("service_tier").and_then(Value::as_str);
        let service_tier = match &options.resolve_service_tier {
            Some(resolve) => resolve(response_tier, options.service_tier.as_deref()),
            None => response_tier
                .map(str::to_string)
                .or_else(|| options.service_tier.clone()),
        };
        apply_pricing(&mut output.usage, service_tier.as_deref());
    }
    // Map status to stop reason; retain the provider's specific reason for
    // incomplete responses.
    let status = response.get("status").and_then(Value::as_str);
    let incomplete_reason = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    output.raw_stop_reason = Some(match incomplete_reason {
        Some(reason) => format!("{}.{reason}", status.unwrap_or_default()),
        None => status.unwrap_or_default().to_string(),
    });
    let (stop_reason, error_message) = map_stop_reason(status, incomplete_reason)?;
    output.stop_reason = stop_reason;
    output.error_message = error_message;
    if output
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        && output.stop_reason == StopReason::Stop
    {
        output.stop_reason = StopReason::ToolUse;
    }
    Ok(())
}
