//! Port of `pi-core/ai/src/api/google-shared.ts`: shared helpers for the
//! Google Generative AI and Vertex providers.
//!
//! The Google GenAI SDK types become plain JSON values; the SDK request
//! wrapper reduces to the direct transport call plus the shared retry
//! policy.

use serde_json::{Map, Value, json};

use crate::ai::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::ai::api::transform_messages::transform_messages;
use crate::ai::types::{
    Context, Message, Model, ModelThinkingLevel, StopReason, ThinkingLevel, Tool,
};
use crate::ai::utils::sanitize_unicode::sanitize_surrogates;

/// Google API kinds.
pub type GoogleApiType = str;

/// Port of `resolveGoogleThinkingLevel`: resolves a supported pi level or
/// model-specific Google mapping to a standard Google level.
pub fn resolve_google_thinking_level(
    model: &Model,
    level: ModelThinkingLevel,
) -> Result<ThinkingLevel, String> {
    if level == ModelThinkingLevel::Off {
        return Ok(ThinkingLevel::High);
    }

    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
        .cloned()
        .flatten();
    let resolved_level = match &mapped {
        Some(value) => value.to_lowercase(),
        None => match level {
            ModelThinkingLevel::Minimal => "minimal".to_string(),
            ModelThinkingLevel::Low => "low".to_string(),
            ModelThinkingLevel::Medium => "medium".to_string(),
            ModelThinkingLevel::High => "high".to_string(),
            ModelThinkingLevel::Xhigh => "xhigh".to_string(),
            ModelThinkingLevel::Max => "max".to_string(),
            ModelThinkingLevel::Off => unreachable!("off handled above"),
        },
    };
    let level_name = model_thinking_level_name(level);
    match resolved_level.as_str() {
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        _ => Err(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
            model.provider,
            model.id,
            level_name,
            mapped.as_deref().unwrap_or("undefined")
        )),
    }
}

/// The TypeScript level strings used in the unsupported-mapping message.
fn model_thinking_level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

/// Port of `isThinkingPart`: `thought: true` is the definitive marker for
/// thinking content.
pub fn is_thinking_part(part: &Value) -> bool {
    part.get("thought") == Some(&json!(true))
}

/// Port of `retainThoughtSignature`: preserves the last non-empty signature
/// for the current block.
pub fn retain_thought_signature(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(value) if !value.is_empty() => Some(value.to_string()),
        _ => existing.map(str::to_string),
    }
}

/// Thought signatures must be base64 for Google APIs (TYPE_BYTES).
fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    let Some(signature) = signature else {
        return false;
    };
    if signature.len() % 4 != 0 {
        return false;
    }
    !signature.is_empty()
        && signature
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '=')
}

/// Only keep signatures from the same provider/model and with valid base64.
fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    if is_same_provider_and_model && is_valid_thought_signature(signature) {
        Some(signature.unwrap_or_default().to_string())
    } else {
        None
    }
}

fn get_gemini_major_version(model_id: &str) -> Option<u32> {
    // Port of /^gemini(?:-live)?-(\d+)/ on the lowercased id.
    let lowered = model_id.to_lowercase();
    let rest = lowered
        .strip_prefix("gemini-")
        .or_else(|| lowered.strip_prefix("gemini"))?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Models via Google APIs that require explicit tool call IDs in function
/// calls/responses.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    let gemini_major_version = get_gemini_major_version(model_id);
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version.is_some_and(|version| version >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    match get_gemini_major_version(model_id) {
        Some(version) => version >= 3,
        None => true,
    }
}

/// A Gemini `Content` entry (role + parts) as raw JSON.
type Content = Value;

/// Port of `convertMessages`: converts internal messages to Gemini
/// Content[] format.
#[allow(clippy::too_many_lines)]
pub fn convert_messages(model: &Model, context: &Context) -> Vec<Content> {
    let mut contents: Vec<Content> = Vec::new();
    let normalize_tool_call_id =
        |id: &str, _source: &crate::ai::types::AssistantMessage| -> String {
            if !requires_tool_call_id(&model.id) {
                return id.to_string();
            }
            let cleaned: String = id
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            cleaned.chars().take(64).collect()
        };

    let transformed_messages =
        transform_messages(&context.messages, model, Some(&normalize_tool_call_id));

    for message in &transformed_messages {
        match message {
            Message::User(user) => match &user.content {
                crate::ai::types::UserContent::Text(text) => {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"text": sanitize_surrogates(text)}],
                    }));
                }
                crate::ai::types::UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            crate::ai::types::BlockContent::Text(text) => {
                                json!({ "text": sanitize_surrogates(&text.text) })
                            }
                            crate::ai::types::BlockContent::Image(image) => json!({
                                "inlineData": {
                                    "mimeType": image.mime_type,
                                    "data": image.data,
                                },
                            }),
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut parts: Vec<Value> = Vec::new();
                // Only keep thinking blocks from the same provider and model.
                let is_same_provider_and_model =
                    assistant.provider == model.provider && assistant.model == model.id;

                for block in &assistant.content {
                    match block {
                        crate::ai::types::AssistantContent::Text(text) => {
                            let thought_signature = resolve_thought_signature(
                                is_same_provider_and_model,
                                text.text_signature.as_deref(),
                            );
                            // Skip empty text blocks unless they carry a
                            // thought signature (Gemini requires the
                            // signature echoed back).
                            if (text.text.is_empty() || text.text.trim().is_empty())
                                && thought_signature.is_none()
                            {
                                continue;
                            }
                            let mut part = json!({ "text": sanitize_surrogates(&text.text) });
                            if let Some(thought_signature) = thought_signature {
                                part["thoughtSignature"] = json!(thought_signature);
                            }
                            parts.push(part);
                        }
                        crate::ai::types::AssistantContent::Thinking(thinking) => {
                            if is_same_provider_and_model {
                                let thought_signature = resolve_thought_signature(
                                    is_same_provider_and_model,
                                    thinking.thinking_signature.as_deref(),
                                );
                                // An empty thinking block is dropped only
                                // when it carries no signature.
                                if (thinking.thinking.is_empty()
                                    || thinking.thinking.trim().is_empty())
                                    && thought_signature.is_none()
                                {
                                    continue;
                                }
                                let mut part = json!({
                                    "thought": true,
                                    "text": sanitize_surrogates(&thinking.thinking),
                                });
                                if let Some(thought_signature) = thought_signature {
                                    part["thoughtSignature"] = json!(thought_signature);
                                }
                                parts.push(part);
                            } else {
                                // Cross-provider/model: the signature is
                                // unusable, empty blocks stay dropped.
                                if thinking.thinking.is_empty()
                                    || thinking.thinking.trim().is_empty()
                                {
                                    continue;
                                }
                                parts.push(
                                    json!({ "text": sanitize_surrogates(&thinking.thinking) }),
                                );
                            }
                        }
                        crate::ai::types::AssistantContent::ToolCall(tool_call) => {
                            let thought_signature = resolve_thought_signature(
                                is_same_provider_and_model,
                                tool_call.thought_signature.as_deref(),
                            );
                            let mut function_call = json!({
                                "name": tool_call.name,
                                "args": Value::Object(tool_call.arguments.clone()),
                            });
                            if requires_tool_call_id(&model.id) {
                                function_call["id"] = json!(tool_call.id);
                            }
                            let mut part = json!({ "functionCall": function_call });
                            if let Some(thought_signature) = thought_signature {
                                part["thoughtSignature"] = json!(thought_signature);
                            }
                            parts.push(part);
                        }
                    }
                }

                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({
                    "role": "model",
                    "parts": parts,
                }));
            }
            Message::ToolResult(result) => {
                let text_result = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        crate::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let image_content: Vec<&crate::ai::types::ImageContent> =
                    if model.input.contains(&crate::ai::types::ModelInput::Image) {
                        result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                crate::ai::types::BlockContent::Image(image) => Some(image),
                                _ => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                let has_text = !text_result.is_empty();
                let has_images = !image_content.is_empty();

                // Gemini 3+ models support multimodal function responses with
                // images nested inside functionResponse.parts.
                let model_supports_multimodal_function_response =
                    supports_multimodal_function_response(&model.id);

                let response_value = if has_text {
                    sanitize_surrogates(&text_result)
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };

                let image_parts: Vec<Value> = image_content
                    .iter()
                    .map(|image| {
                        json!({
                            "inlineData": {
                                "mimeType": image.mime_type,
                                "data": image.data,
                            },
                        })
                    })
                    .collect();

                let include_id = requires_tool_call_id(&model.id);
                let mut function_response = json!({
                    "name": result.tool_name,
                    "response": if result.is_error {
                        json!({ "error": response_value })
                    } else {
                        json!({ "output": response_value })
                    },
                });
                if has_images && model_supports_multimodal_function_response {
                    function_response["parts"] = json!(image_parts);
                }
                if include_id {
                    function_response["id"] = json!(result.tool_call_id);
                }
                let function_response_part = json!({ "functionResponse": function_response });

                // Cloud Code Assist API requires all function responses in a
                // single user turn; merge into the last user turn when it
                // already carries function responses.
                let last_content = contents.last_mut();
                let merged = match last_content {
                    Some(content)
                        if content.get("role").and_then(Value::as_str) == Some("user")
                            && content.get("parts").and_then(Value::as_array).is_some_and(
                                |parts| {
                                    parts
                                        .iter()
                                        .any(|part| part.get("functionResponse").is_some())
                                },
                            ) =>
                    {
                        if let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut)
                        {
                            parts.push(function_response_part.clone());
                        }
                        true
                    }
                    _ => false,
                };
                if !merged {
                    contents.push(json!({
                        "role": "user",
                        "parts": [function_response_part],
                    }));
                }

                // For Gemini < 3, add images in a separate user message.
                if has_images && !model_supports_multimodal_function_response {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(image_parts);
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            }
        }
    }

    contents
}

const JSON_SCHEMA_META_DECLARATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions", // pre-draft-2019-09 equivalent of $defs
];

/// Port of `sanitizeForOpenApi`: strips meta-declarations from a schema.
fn sanitize_for_open_api(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return schema.clone();
    };
    let mut result = Map::new();
    for (key, value) in object {
        if JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()) {
            continue;
        }
        result.insert(key.clone(), sanitize_for_open_api(value));
    }
    Value::Object(result)
}

/// Port of `convertTools`: converts tools to Gemini function declarations.
/// `use_parameters` selects the legacy OpenAPI 3.03 `parameters` field
/// (needed for Cloud Code Assist with Claude models).
pub fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict_mode: bool,
) -> Result<Option<Vec<Value>>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
        let parameters = get_json_schema_tool_parameters(tool, strict == Some(true))?;
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), json!(tool.name));
        declaration.insert("description".to_string(), json!(tool.description));
        if use_parameters {
            declaration.insert("parameters".to_string(), sanitize_for_open_api(&parameters));
        } else {
            declaration.insert("parametersJsonSchema".to_string(), parameters);
        }
        declarations.push(Value::Object(declaration));
    }
    Ok(Some(vec![json!({ "functionDeclarations": declarations })]))
}

/// Port of `supportsGoogleStrictToolSampling`: Gemini 3+ enforces required
/// function parameters in validated tool-calling modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    get_gemini_major_version(model_id).is_some_and(|version| version >= 3)
}

/// Port of `FunctionCallingConfigMode` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionCallingConfigMode {
    Auto,
    None,
    Any,
    Validated,
}

impl FunctionCallingConfigMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FunctionCallingConfigMode::Auto => "AUTO",
            FunctionCallingConfigMode::None => "NONE",
            FunctionCallingConfigMode::Any => "ANY",
            FunctionCallingConfigMode::Validated => "VALIDATED",
        }
    }
}

/// Port of `mapToolChoice`.
pub fn map_tool_choice(choice: &str) -> FunctionCallingConfigMode {
    match choice {
        "auto" => FunctionCallingConfigMode::Auto,
        "none" => FunctionCallingConfigMode::None,
        "any" => FunctionCallingConfigMode::Any,
        _ => FunctionCallingConfigMode::Auto,
    }
}

/// Port of `resolveGoogleFunctionCallingMode`.
pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<FunctionCallingConfigMode>, String> {
    // TypeScript's `tools.some(...)` propagates a throwing resolver, so the
    // require-strict error must escape instead of being treated as `false`.
    let mut use_strict_mode = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict_mode)? == Some(true) {
            use_strict_mode = true;
            break;
        }
    }
    if matches!(tool_choice, Some("none") | Some("any")) {
        return Ok(Some(map_tool_choice(tool_choice.expect("checked"))));
    }
    if use_strict_mode {
        return Ok(Some(FunctionCallingConfigMode::Validated));
    }
    Ok(tool_choice.map(map_tool_choice))
}

/// Port of `mapStopReason` for raw finish-reason strings.
pub fn map_stop_reason_string(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}
