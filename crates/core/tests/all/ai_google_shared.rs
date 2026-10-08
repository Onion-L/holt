//! Port of the offline google-shared TS suites:
//! - `google-shared-convert-tools.test.ts`
//! - `google-shared-gemini3-unsigned-tool-call.test.ts`
//! - `google-shared-image-tool-result-routing.test.ts`
//! - `google-shared-signed-empty-blocks.test.ts`
//! - `google-thinking-signature.test.ts`
//! - `google-thinking-level-map.test.ts`
//!
//! The thinking-level-map payload cases mock `@google/genai` in TypeScript;
//! the Rust port captures the request payload through the `onPayload` hook
//! and fails the mocked transport with the same "payload captured" error the
//! TS `onPayload` callbacks throw.

use std::sync::{Arc, Mutex};

use pi_core::ai::api::google_generative_ai::stream_simple_with_transport as stream_simple_google;
use pi_core::ai::api::google_shared::{
    FunctionCallingConfigMode, convert_messages, convert_tools, is_thinking_part,
    requires_tool_call_id, resolve_google_function_calling_mode, resolve_google_thinking_level,
    retain_thought_signature, supports_google_strict_tool_sampling,
};
use pi_core::ai::api::google_vertex::stream_simple_with_transport as stream_simple_vertex;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, ConstrainedSamplingConfig,
    ConstrainedSamplingStrict, Context, ImageContent, Message, Model, ModelCost, ModelInput,
    ModelThinkingLevel, OnPayloadCallback, RoleAssistant, RoleToolResult, RoleUser,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingBudgets, ThinkingContent,
    ThinkingLevel, ThinkingLevelMap, Tool, ToolCall, ToolConstrainedSampling, ToolResultMessage,
    Usage, UserContent, UserMessage,
};
use pi_core::ai::utils::http::{HttpFetch, HttpRequest, HttpResponse};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// google-shared-convert-tools.test.ts
// ---------------------------------------------------------------------------

fn make_tool(parameters: Value) -> Tool {
    Tool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        parameters,
        constrained_sampling: None,
    }
}

#[test]
fn strips_json_schema_meta_keys_from_parameters_when_use_parameters_is_true() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "urn:bash-tool",
        "$comment": "A bash tool for demonstration",
        "$defs": {
            "commandDef": { "type": "string" },
        },
        "definitions": {
            "legacyDef": { "type": "number" },
        },
        "type": "object",
        "properties": {
            "command": { "type": "string" },
        },
        "required": ["command"],
    }))];

    let result = convert_tools(&tools, true, true)
        .unwrap()
        .expect("declaration list");
    let decl = &result[0]["functionDeclarations"][0];
    assert!(decl.is_object());
    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
            },
            "required": ["command"],
        })
    );
    for key in ["$schema", "$id", "$comment", "$defs", "definitions"] {
        assert!(decl["parameters"].get(key).is_none(), "{key} was stripped");
    }
}

#[test]
fn recursively_strips_nested_json_schema_meta_keys() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "deep": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$id": "urn:nested",
                "type": "string",
            },
        },
    }))];

    let result = convert_tools(&tools, true, true)
        .unwrap()
        .expect("declaration list");
    let decl = &result[0]["functionDeclarations"][0];
    assert!(decl.is_object());
    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {
                "deep": {
                    "type": "string",
                },
            },
        })
    );
}

#[test]
fn preserves_ref_while_stripping_meta_keys() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "refProp": {
                "$ref": "#/$defs/someDef",
                "type": "string",
            },
        },
    }))];

    let result = convert_tools(&tools, true, true)
        .unwrap()
        .expect("declaration list");
    let decl = &result[0]["functionDeclarations"][0];
    assert!(decl.is_object());
    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {
                "refProp": {
                    "$ref": "#/$defs/someDef",
                    "type": "string",
                },
            },
        })
    );
}

#[test]
fn does_not_mutate_the_original_tool_parameters_object() {
    let original_parameters = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "command": { "type": "string" },
        },
        "required": ["command"],
    });
    let tools = vec![make_tool(original_parameters.clone())];

    let _ = convert_tools(&tools, true, true).unwrap();

    assert_eq!(
        original_parameters,
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "command": { "type": "string" },
            },
            "required": ["command"],
        })
    );
}

#[test]
fn preserves_schema_in_parameters_json_schema_when_use_parameters_is_false() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "command": { "type": "string" },
        },
        "required": ["command"],
    }))];

    let result = convert_tools(&tools, false, true)
        .unwrap()
        .expect("declaration list");
    let decl = &result[0]["functionDeclarations"][0];
    assert!(decl.is_object());
    assert_eq!(
        decl["parametersJsonSchema"],
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "command": { "type": "string" },
            },
            "required": ["command"],
        })
    );
}

#[test]
fn handles_tools_without_schema_gracefully() {
    let tools = vec![make_tool(json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
        },
        "required": ["path"],
    }))];

    let result = convert_tools(&tools, true, true)
        .unwrap()
        .expect("declaration list");
    let decl = &result[0]["functionDeclarations"][0];
    assert!(decl.is_object());
    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
            },
            "required": ["path"],
        })
    );
}

#[test]
fn uses_validated_function_calling_for_strict_tools_on_gemini_3() {
    let mut tool = make_tool(json!({ "type": "object", "properties": {} }));
    tool.constrained_sampling = Some(ToolConstrainedSampling::Config(
        ConstrainedSamplingConfig::JsonSchema {
            strict: ConstrainedSamplingStrict::Require,
        },
    ));

    assert!(supports_google_strict_tool_sampling(
        "gemini-3.1-pro-preview"
    ));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));
    assert_eq!(
        resolve_google_function_calling_mode(&[tool.clone()], None, true).unwrap(),
        Some(FunctionCallingConfigMode::Validated)
    );
    let error = resolve_google_function_calling_mode(&[tool], None, false).unwrap_err();
    assert!(
        error.contains(r#"Tool "test_tool" requires JSON-schema constrained sampling"#),
        "unexpected error: {error}"
    );
}

#[test]
fn returns_undefined_for_empty_tool_list() {
    assert!(convert_tools(&[], false, true).unwrap().is_none());
    assert!(convert_tools(&[], true, true).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// google-shared-gemini3-unsigned-tool-call.test.ts
// ---------------------------------------------------------------------------

fn make_gemini3_model(api: &str, provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: "Gemini 3 Pro Preview".to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 8_192,
        ..Default::default()
    }
}

fn make_unsigned_tool_call_context(
    api: &str,
    provider: &str,
    model_id: &str,
    thought_signature: Option<&str>,
) -> Context {
    let now = 1_700_000_000_000_i64;
    let mut call_1 = ToolCall {
        id: "call_1".to_string(),
        name: "bash".to_string(),
        arguments: json!({ "command": "echo hi" })
            .as_object()
            .cloned()
            .unwrap(),
        ..Default::default()
    };
    if let Some(thought_signature) = thought_signature {
        call_1.thought_signature = Some(thought_signature.to_string());
    }
    Context {
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("Hi".to_string()),
                timestamp: now,
            }),
            Message::Assistant(Box::new(AssistantMessage {
                role: RoleAssistant,
                content: vec![
                    AssistantContent::ToolCall(call_1),
                    AssistantContent::ToolCall(ToolCall {
                        id: "call_2".to_string(),
                        name: "bash".to_string(),
                        arguments: json!({ "command": "ls -la" }).as_object().cloned().unwrap(),
                        ..Default::default()
                    }),
                ],
                api: api.to_string(),
                provider: provider.to_string(),
                model: model_id.to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                timestamp: now,
                ..Default::default()
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: RoleToolResult,
                tool_call_id: "call_1".to_string(),
                tool_name: "bash".to_string(),
                content: vec![BlockContent::Text(TextContent {
                    text: "hi".to_string(),
                    ..Default::default()
                })],
                is_error: false,
                timestamp: now,
                ..Default::default()
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: RoleToolResult,
                tool_call_id: "call_2".to_string(),
                tool_name: "bash".to_string(),
                content: vec![BlockContent::Text(TextContent {
                    text: "files".to_string(),
                    ..Default::default()
                })],
                is_error: false,
                timestamp: now,
                ..Default::default()
            })),
        ],
        ..Default::default()
    }
}

fn contents_parts(contents: &[Value]) -> Vec<&Value> {
    contents
        .iter()
        .filter_map(|content| content.get("parts")?.as_array())
        .flatten()
        .collect()
}

fn model_turn(contents: &[Value]) -> &Value {
    contents
        .iter()
        .find(|content| content.get("role") == Some(&json!("model")))
        .expect("model turn")
}

fn assert_tool_call_ids_preserved(model: &Model) {
    let context = make_unsigned_tool_call_context(&model.api, &model.provider, &model.id, None);
    let contents = convert_messages(model, &context);
    let function_call_ids: Vec<&str> = contents_parts(&contents)
        .into_iter()
        .filter_map(|part| part.pointer("/functionCall/id").and_then(Value::as_str))
        .collect();
    let function_response_ids: Vec<&str> = contents_parts(&contents)
        .into_iter()
        .filter_map(|part| part.pointer("/functionResponse/id").and_then(Value::as_str))
        .collect();

    assert_eq!(function_call_ids, vec!["call_1", "call_2"]);
    assert_eq!(function_response_ids, vec!["call_1", "call_2"]);
}

#[test]
fn preserves_tool_call_ids_for_gemini_3_pro_preview_via_google_generative_ai_history() {
    let model = make_gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview");
    assert_tool_call_ids_preserved(&model);
}

#[test]
fn preserves_tool_call_ids_for_gemini_3_6_flash_via_google_generative_ai_history() {
    let model = make_gemini3_model("google-generative-ai", "google", "gemini-3.6-flash");
    assert_tool_call_ids_preserved(&model);
}

#[test]
fn preserves_tool_call_ids_for_gemini_3_pro_preview_via_google_vertex_history() {
    let model = make_gemini3_model("google-vertex", "google-vertex", "gemini-3-pro-preview");
    assert_tool_call_ids_preserved(&model);
}

#[test]
fn does_not_add_skip_thought_signature_validator_for_unsigned_google_gen_ai_tool_calls() {
    let model = make_gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &make_unsigned_tool_call_context(&model.api, &model.provider, "other-model", None),
    );

    let model_turn = model_turn(&contents);
    let function_call_parts: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect();
    assert_eq!(function_call_parts.len(), 2);
    assert!(function_call_parts[0].get("thoughtSignature").is_none());
    assert!(function_call_parts[1].get("thoughtSignature").is_none());
    assert!(
        !serde_json::to_string(model_turn)
            .unwrap()
            .contains("skip_thought_signature_validator")
    );

    let historical_text = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Historical context"))
        })
        .count();
    assert_eq!(historical_text, 0);
}

#[test]
fn does_not_add_skip_thought_signature_validator_for_unsigned_vertex_tool_calls() {
    let model = make_gemini3_model("google-vertex", "google-vertex", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &make_unsigned_tool_call_context(&model.api, &model.provider, &model.id, None),
    );
    let model_turn = model_turn(&contents);
    let function_call_parts: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert!(function_call_parts[0].get("thoughtSignature").is_none());
    assert!(function_call_parts[1].get("thoughtSignature").is_none());
    assert!(
        !serde_json::to_string(model_turn)
            .unwrap()
            .contains("skip_thought_signature_validator")
    );
}

#[test]
fn preserves_valid_thought_signature_when_present_for_the_same_provider_and_model() {
    let model = make_gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let valid_sig = "AAAAAAAAAAAAAAAAAAAAAA==";
    let contents = convert_messages(
        &model,
        &make_unsigned_tool_call_context(&model.api, &model.provider, &model.id, Some(valid_sig)),
    );
    let model_turn = model_turn(&contents);
    let function_call_parts: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert_eq!(
        function_call_parts[0].get("thoughtSignature"),
        Some(&json!(valid_sig))
    );
    assert!(function_call_parts[1].get("thoughtSignature").is_none());
}

#[test]
fn does_not_add_a_thought_signature_for_non_gemini_3_models() {
    let model = make_gemini3_model("google-generative-ai", "google", "gemini-2.5-flash");
    let contents = convert_messages(
        &model,
        &make_unsigned_tool_call_context(&model.api, &model.provider, "other-model", None),
    );
    let model_turn = model_turn(&contents);
    let function_call_parts: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect();
    let function_response_parts: Vec<&Value> = contents_parts(&contents)
        .into_iter()
        .filter(|part| part.get("functionResponse").is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert!(
        function_call_parts
            .iter()
            .all(|part| part.pointer("/functionCall/id").is_none())
    );
    assert!(
        function_call_parts
            .iter()
            .all(|part| part.get("thoughtSignature").is_none())
    );
    assert_eq!(function_response_parts.len(), 2);
    assert!(
        function_response_parts
            .iter()
            .all(|part| part.pointer("/functionResponse/id").is_none())
    );
}

// requiresToolCallId it.each
#[test]
fn requires_tool_call_id_returns_false_for_gemini_2_5_flash() {
    assert!(!requires_tool_call_id("gemini-2.5-flash"));
}

#[test]
fn requires_tool_call_id_returns_true_for_gemini_3_6_flash() {
    assert!(requires_tool_call_id("gemini-3.6-flash"));
}

#[test]
fn requires_tool_call_id_returns_true_for_claude_sonnet_4_5() {
    assert!(requires_tool_call_id("claude-sonnet-4-5"));
}

#[test]
fn requires_tool_call_id_returns_true_for_gpt_oss_120b() {
    assert!(requires_tool_call_id("gpt-oss-120b"));
}

// ---------------------------------------------------------------------------
// google-shared-image-tool-result-routing.test.ts
// ---------------------------------------------------------------------------

fn make_image_routing_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "google-generative-ai".to_string(),
        provider: "google".to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 8_192,
        ..Default::default()
    }
}

fn make_image_routing_context(api: &str, provider: &str, model_id: &str) -> Context {
    let now = 1_700_000_000_000_i64;
    let tool_call = |id: &str, path: &str| {
        AssistantContent::ToolCall(ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": path }).as_object().cloned().unwrap(),
            ..Default::default()
        })
    };
    let text_result = |id: &str, text: &str| {
        Message::ToolResult(Box::new(ToolResultMessage {
            role: RoleToolResult,
            tool_call_id: id.to_string(),
            tool_name: "read".to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: text.to_string(),
                ..Default::default()
            })],
            is_error: false,
            timestamp: now,
            ..Default::default()
        }))
    };
    Context {
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("read the files".to_string()),
                timestamp: now,
            }),
            Message::Assistant(Box::new(AssistantMessage {
                role: RoleAssistant,
                content: vec![
                    tool_call("call_a", "a.txt"),
                    tool_call("call_img", "image.png"),
                    tool_call("call_b", "b.txt"),
                ],
                api: api.to_string(),
                provider: provider.to_string(),
                model: model_id.to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                timestamp: now,
                ..Default::default()
            })),
            text_result("call_a", "alpha text"),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: RoleToolResult,
                tool_call_id: "call_img".to_string(),
                tool_name: "read".to_string(),
                content: vec![BlockContent::Image(ImageContent {
                    data: "abc".to_string(),
                    mime_type: "image/png".to_string(),
                    ..Default::default()
                })],
                is_error: false,
                timestamp: now,
                ..Default::default()
            })),
            text_result("call_b", "beta text"),
        ],
        ..Default::default()
    }
}

#[test]
fn keeps_separate_synthetic_image_turn_for_gemini_2_x_google_api_models() {
    let model = make_image_routing_model("gemini-2.5-flash");
    let contents = convert_messages(
        &model,
        &make_image_routing_context(&model.api, &model.provider, &model.id),
    );

    assert_eq!(contents.len(), 5);
    assert!(
        contents_parts(std::slice::from_ref(&contents[2]))
            .iter()
            .all(|part| part.get("functionResponse").is_some())
    );
    assert_eq!(
        contents[3]["parts"][0].get("text"),
        Some(&json!("Tool result image:"))
    );
    assert!(contents[3]["parts"][1].get("inlineData").is_some());
    assert!(contents[4]["parts"][0].get("functionResponse").is_some());
}

#[test]
fn nests_image_tool_results_for_gemini_3_google_api_models() {
    let model = make_image_routing_model("gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &make_image_routing_context(&model.api, &model.provider, &model.id),
    );

    assert_eq!(contents.len(), 3);
    let tool_result_turn = &contents[2];
    assert_eq!(tool_result_turn["parts"].as_array().map(Vec::len), Some(3));
    let image_response = &tool_result_turn["parts"][1]["functionResponse"];
    assert!(image_response.is_object());
    assert_eq!(
        image_response["parts"].as_array().map(Vec::len),
        Some(1),
        "image response nests a single inlineData part"
    );
    assert!(image_response["parts"][0].get("inlineData").is_some());
}

// ---------------------------------------------------------------------------
// google-shared-signed-empty-blocks.test.ts
// ---------------------------------------------------------------------------

// Gemini can attach `thoughtSignature` to a response part whose visible text
// is empty (e.g. a thought burst preceding a function call) and requires the
// signature echoed back on the next request. Dropping such blocks while
// rebuilding history silently breaks the reasoning chain: the model then
// intermittently ends a mid-task turn with a thought-only STOP and no tool
// call. These tests pin the rule: an empty text/thinking block is skipped
// only when it is UNSIGNED.

const VALID_SIG: &str = "AAAAAAAAAAAAAAAAAAAAAA==";

fn make_signed_blocks_model() -> Model {
    Model {
        id: "gemini-3-pro-preview".to_string(),
        name: "gemini-3-pro-preview".to_string(),
        api: "google-generative-ai".to_string(),
        provider: "google".to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: true,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 8_192,
        ..Default::default()
    }
}

fn make_signed_blocks_context(
    api: &str,
    provider: &str,
    model_id: &str,
    content: Vec<AssistantContent>,
) -> Context {
    let now = 1_700_000_000_000_i64;
    Context {
        messages: vec![
            Message::User(UserMessage {
                role: RoleUser,
                content: UserContent::Text("Hi".to_string()),
                timestamp: now,
            }),
            Message::Assistant(Box::new(AssistantMessage {
                role: RoleAssistant,
                content,
                api: api.to_string(),
                provider: provider.to_string(),
                model: model_id.to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                timestamp: now,
                ..Default::default()
            })),
        ],
        ..Default::default()
    }
}

fn bash_tool_call() -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: "call_1".to_string(),
        name: "bash".to_string(),
        arguments: json!({ "command": "ls" }).as_object().cloned().unwrap(),
        ..Default::default()
    })
}

#[test]
fn keeps_a_signed_empty_thinking_block_so_its_signature_is_echoed_back() {
    let model = make_signed_blocks_model();
    let contents = convert_messages(
        &model,
        &make_signed_blocks_context(
            &model.api,
            &model.provider,
            &model.id,
            vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some(VALID_SIG.to_string()),
                    ..Default::default()
                }),
                bash_tool_call(),
            ],
        ),
    );
    let model_turn = model_turn(&contents);
    let signed: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("thoughtSignature") == Some(&json!(VALID_SIG)))
        .collect();
    assert_eq!(signed.len(), 1);
    assert_eq!(signed[0].get("thought"), Some(&json!(true)));
}

#[test]
fn keeps_a_signed_empty_text_block_the_same_way() {
    let model = make_signed_blocks_model();
    let contents = convert_messages(
        &model,
        &make_signed_blocks_context(
            &model.api,
            &model.provider,
            &model.id,
            vec![
                AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: Some(VALID_SIG.to_string()),
                    ..Default::default()
                }),
                bash_tool_call(),
            ],
        ),
    );
    let model_turn = model_turn(&contents);
    let signed: Vec<&Value> = contents_parts(std::slice::from_ref(model_turn))
        .into_iter()
        .filter(|part| part.get("thoughtSignature") == Some(&json!(VALID_SIG)))
        .collect();
    assert_eq!(signed.len(), 1);
}

#[test]
fn still_drops_unsigned_empty_blocks() {
    let model = make_signed_blocks_model();
    let contents = convert_messages(
        &model,
        &make_signed_blocks_context(
            &model.api,
            &model.provider,
            &model.id,
            vec![
                AssistantContent::Thinking(ThinkingContent::default()),
                AssistantContent::Text(TextContent {
                    text: "   ".to_string(),
                    ..Default::default()
                }),
                bash_tool_call(),
            ],
        ),
    );
    let model_turn = model_turn(&contents);
    let parts = contents_parts(std::slice::from_ref(model_turn));
    assert_eq!(parts.len(), 1);
    assert!(parts[0].get("functionCall").is_some());
}

#[test]
fn still_drops_signed_empty_blocks_from_a_different_provider_model() {
    let model = make_signed_blocks_model();
    let contents = convert_messages(
        &model,
        &make_signed_blocks_context(
            &model.api,
            &model.provider,
            "other-model",
            vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some(VALID_SIG.to_string()),
                    ..Default::default()
                }),
                AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: Some(VALID_SIG.to_string()),
                    ..Default::default()
                }),
                bash_tool_call(),
            ],
        ),
    );
    let model_turn = model_turn(&contents);
    let parts = contents_parts(std::slice::from_ref(model_turn));
    assert_eq!(parts.len(), 1);
    assert!(parts[0].get("functionCall").is_some());
    assert!(
        !serde_json::to_string(model_turn)
            .unwrap()
            .contains(VALID_SIG)
    );
}

// ---------------------------------------------------------------------------
// google-thinking-signature.test.ts
// ---------------------------------------------------------------------------

#[test]
fn treats_part_thought_true_as_thinking() {
    assert!(is_thinking_part(&json!({ "thought": true })));
    assert!(is_thinking_part(
        &json!({ "thought": true, "thoughtSignature": "opaque-signature" })
    ));
}

#[test]
fn does_not_treat_thought_signature_alone_as_thinking() {
    // Per Google docs, thoughtSignature is for context replay and can appear
    // on any part type. Only thought === true indicates thinking content.
    // See: https://ai.google.dev/gemini-api/docs/thought-signatures
    assert!(!is_thinking_part(
        &json!({ "thoughtSignature": "opaque-signature" })
    ));
    assert!(!is_thinking_part(
        &json!({ "thought": false, "thoughtSignature": "opaque-signature" })
    ));
}

#[test]
fn does_not_treat_empty_or_missing_signatures_as_thinking_if_thought_is_not_set() {
    assert!(!is_thinking_part(&json!({})));
    assert!(!is_thinking_part(
        &json!({ "thought": false, "thoughtSignature": "" })
    ));
}

#[test]
fn preserves_the_existing_signature_when_subsequent_deltas_omit_thought_signature() {
    let first = retain_thought_signature(None, Some("sig-1"));
    assert_eq!(first.as_deref(), Some("sig-1"));

    let second = retain_thought_signature(first.as_deref(), None);
    assert_eq!(second.as_deref(), Some("sig-1"));

    let third = retain_thought_signature(second.as_deref(), Some(""));
    assert_eq!(third.as_deref(), Some("sig-1"));
}

#[test]
fn updates_the_signature_when_a_new_non_empty_signature_arrives() {
    let updated = retain_thought_signature(Some("sig-1"), Some("sig-2"));
    assert_eq!(updated.as_deref(), Some("sig-2"));
}

// ---------------------------------------------------------------------------
// google-thinking-level-map.test.ts
// ---------------------------------------------------------------------------

fn capture_context() -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text("Hello".to_string()),
            timestamp: 0,
        })],
        ..Default::default()
    }
}

fn google_model(id: &str, thinking_level_map: ThinkingLevelMap) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "google-generative-ai".to_string(),
        provider: "test-google".to_string(),
        base_url: "https://example.invalid/v1beta".to_string(),
        reasoning: true,
        thinking_level_map: Some(thinking_level_map),
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        ..Default::default()
    }
}

fn vertex_model(id: &str, thinking_level_map: ThinkingLevelMap) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "google-vertex".to_string(),
        provider: "test-vertex".to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        reasoning: true,
        thinking_level_map: Some(thinking_level_map),
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        ..Default::default()
    }
}

/// The mocked transport that fails the request after the payload was
/// captured, mirroring the TS `onPayload` callbacks that throw
/// "payload captured".
struct PayloadCapturedFetch;

impl HttpFetch for PayloadCapturedFetch {
    fn fetch<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> futures::future::BoxFuture<
        'a,
        Result<HttpResponse, pi_core::ai::utils::http::HttpFetchError>,
    > {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from_static(
                    b"event: __error__\ndata: payload captured\n\n",
                ))])),
            })
        })
    }
}

async fn capture_google_payload(
    model: &Model,
    reasoning: ThinkingLevel,
    thinking_budgets: Option<ThinkingBudgets>,
) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let on_payload: OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(Arc::new(PayloadCapturedFetch)),
                on_payload: Some(on_payload),
                ..Default::default()
            },
            ..Default::default()
        },
        reasoning: Some(reasoning),
        thinking_budgets,
        ..Default::default()
    };

    let result = stream_simple_google(model, &capture_context(), Some(&options), None)
        .result()
        .await;

    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("payload captured")
    );
    captured
        .lock()
        .unwrap()
        .take()
        .expect("Google payload was not captured")
}

async fn capture_vertex_payload(
    model: &Model,
    reasoning: ThinkingLevel,
    thinking_budgets: Option<ThinkingBudgets>,
) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let on_payload: OnPayloadCallback = {
        let captured = Arc::clone(&captured);
        Arc::new(move |payload, _model| {
            *captured.lock().unwrap() = Some(payload);
            Box::pin(std::future::ready(None))
        })
    };
    let options = SimpleStreamOptions {
        base: StreamOptions {
            base: pi_core::ai::types::ProviderRequestOptions {
                api_key: Some("test".to_string()),
                fetch: Some(Arc::new(PayloadCapturedFetch)),
                on_payload: Some(on_payload),
                ..Default::default()
            },
            ..Default::default()
        },
        reasoning: Some(reasoning),
        thinking_budgets,
        ..Default::default()
    };

    let result = stream_simple_vertex(model, &capture_context(), Some(&options), None)
        .result()
        .await;

    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("payload captured")
    );
    captured
        .lock()
        .unwrap()
        .take()
        .expect("Vertex payload was not captured")
}

#[test]
fn exhaustively_resolves_supported_logical_levels_and_mapping_values() {
    // Default expectations: every supported pi level resolves without a map.
    let default_expectations = [
        (ModelThinkingLevel::Off, ThinkingLevel::High),
        (ModelThinkingLevel::Minimal, ThinkingLevel::Minimal),
        (ModelThinkingLevel::Low, ThinkingLevel::Low),
        (ModelThinkingLevel::Medium, ThinkingLevel::Medium),
        (ModelThinkingLevel::High, ThinkingLevel::High),
    ];
    for (level, expected) in default_expectations {
        assert_eq!(
            resolve_google_thinking_level(
                &google_model("gemini-3.7-flash", ThinkingLevelMap::new()),
                level
            )
            .unwrap(),
            expected
        );
    }

    // Mapped expectations: provider values map case-insensitively.
    let mapped_expectations = [
        ("minimal", ThinkingLevel::Minimal),
        ("low", ThinkingLevel::Low),
        ("medium", ThinkingLevel::Medium),
        ("high", ThinkingLevel::High),
        ("MINIMAL", ThinkingLevel::Minimal),
        ("LOW", ThinkingLevel::Low),
        ("MEDIUM", ThinkingLevel::Medium),
        ("HIGH", ThinkingLevel::High),
    ];
    for (mapped, expected) in mapped_expectations {
        let model = google_model(
            "gemini-3.7-flash",
            [
                (ModelThinkingLevel::High, Some(mapped.to_string())),
                (ModelThinkingLevel::Xhigh, Some(mapped.to_string())),
                (ModelThinkingLevel::Max, Some(mapped.to_string())),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::High).unwrap(),
            expected
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::Xhigh).unwrap(),
            expected
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::Max).unwrap(),
            expected
        );
    }

    let mut invalid_map = ThinkingLevelMap::new();
    invalid_map.insert(ModelThinkingLevel::Xhigh, Some("extreme".to_string()));
    let invalid_model = google_model("gemini-3.7-flash", invalid_map);
    assert_eq!(
        resolve_google_thinking_level(&invalid_model, ModelThinkingLevel::Xhigh).unwrap_err(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: xhigh -> extreme"
    );
    assert_eq!(
        resolve_google_thinking_level(
            &google_model("gemini-3.7-flash", ThinkingLevelMap::new()),
            ModelThinkingLevel::Max
        )
        .unwrap_err(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: max -> undefined"
    );
}

#[tokio::test]
async fn maps_google_generative_ai_xhigh_to_a_supported_level() {
    let map = [
        (ModelThinkingLevel::Xhigh, Some("high".to_string())),
        (ModelThinkingLevel::Max, Some("high".to_string())),
    ]
    .into_iter()
    .collect();
    let payload = capture_google_payload(
        &google_model("gemini-3.7-flash", map),
        ThinkingLevel::Xhigh,
        None,
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
    );
}

#[tokio::test]
async fn maps_google_generative_ai_max_to_a_supported_level() {
    let map = [
        (ModelThinkingLevel::Xhigh, Some("high".to_string())),
        (ModelThinkingLevel::Max, Some("high".to_string())),
    ]
    .into_iter()
    .collect();
    let payload = capture_google_payload(
        &google_model("gemini-3.7-flash", map),
        ThinkingLevel::Max,
        None,
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
    );
}

#[tokio::test]
async fn honors_uppercase_provider_values_for_standard_google_generative_ai_levels() {
    let mut map = ThinkingLevelMap::new();
    map.insert(ModelThinkingLevel::High, Some("LOW".to_string()));
    let payload = capture_google_payload(
        &google_model("gemini-3.7-flash", map),
        ThinkingLevel::High,
        None,
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"]["thinkingLevel"],
        json!("LOW")
    );
}

#[tokio::test]
async fn uses_mapped_google_generative_ai_levels_for_token_budgets() {
    let mut map = ThinkingLevelMap::new();
    map.insert(ModelThinkingLevel::Xhigh, Some("high".to_string()));
    let payload = capture_google_payload(
        &google_model("gemini-2.5-flash", map),
        ThinkingLevel::Xhigh,
        Some(ThinkingBudgets {
            high: Some(1234),
            ..Default::default()
        }),
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"]["thinkingBudget"],
        json!(1234)
    );
}

#[tokio::test]
async fn maps_google_vertex_extended_levels() {
    let mut map = ThinkingLevelMap::new();
    map.insert(ModelThinkingLevel::Xhigh, Some("high".to_string()));
    let payload = capture_vertex_payload(
        &vertex_model("gemini-3.7-flash", map),
        ThinkingLevel::Xhigh,
        None,
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"],
        json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
    );
}

#[tokio::test]
async fn uses_mapped_google_vertex_levels_for_token_budgets() {
    let mut map = ThinkingLevelMap::new();
    map.insert(ModelThinkingLevel::Max, Some("high".to_string()));
    let payload = capture_vertex_payload(
        &vertex_model("gemini-2.5-flash", map),
        ThinkingLevel::Max,
        Some(ThinkingBudgets {
            high: Some(4321),
            ..Default::default()
        }),
    )
    .await;

    assert_eq!(
        payload["config"]["thinkingConfig"]["thinkingBudget"],
        json!(4321)
    );
}
