//! Port of `pi-core/ai/test/constrained-sampling.test.ts`: strict JSON-schema
//! derivation, grammar-constrained tool sampling, and custom Responses tool
//! call replay/streaming.
//!
//! The TypeScript suite builds schemas with TypeBox; the Rust port spells out
//! the same JSON schema values TypeBox emits (object with `required` listing
//! the non-optional properties, `anyOf` unions).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use pi_core::ai::api::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta, make_strict_json_schema,
    resolve_json_schema_strict_sampling,
};
use pi_core::ai::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, ResponsesStreamOptions,
    convert_responses_messages, convert_responses_tools, process_responses_stream,
};
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent,
    ConstrainedSamplingConfig, ConstrainedSamplingStrict, Context, GrammarFormat, Message, Model,
    ModelCost, ModelInput, RoleAssistant, RoleToolResult, StopReason, TextContent, Tool, ToolCall,
    ToolCallArguments, ToolConstrainedSampling, ToolResultMessage, Usage, UsageCost,
};
use pi_core::ai::utils::event_stream::{collect_events, create_assistant_message_event_stream};

/// Port of `makeModel`.
fn make_model() -> Model {
    Model {
        id: "gpt-test".to_string(),
        name: "GPT Test".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: false,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4_096,
        ..Default::default()
    }
}

/// Port of `makeUsage`.
fn make_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: UsageCost::default(),
        ..Default::default()
    }
}

/// Port of `makeOutput`.
fn make_output() -> AssistantMessage {
    AssistantMessage {
        role: RoleAssistant,
        content: Vec::new(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        model: "gpt-test".to_string(),
        usage: make_usage(),
        stop_reason: StopReason::Pending,
        timestamp: 0,
        ..Default::default()
    }
}

/// Port of `makeTool`: `Type.Object({ payload: Type.String() },
/// { additionalProperties: false })` as emitted by TypeBox.
fn make_tool() -> Tool {
    Tool {
        name: "sample_tool".to_string(),
        description: "Sample tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "payload": { "type": "string" },
            },
            "additionalProperties": false,
            "required": ["payload"],
        }),
        constrained_sampling: None,
    }
}

fn json_schema_config(strict: ConstrainedSamplingStrict) -> Option<ToolConstrainedSampling> {
    Some(ToolConstrainedSampling::Config(
        ConstrainedSamplingConfig::JsonSchema { strict },
    ))
}

fn grammar_config(variants: &[(GrammarFormat, &str)]) -> Option<ToolConstrainedSampling> {
    Some(ToolConstrainedSampling::Config(
        ConstrainedSamplingConfig::Grammar {
            variants: variants
                .iter()
                .map(|(format, definition)| (*format, definition.to_string()))
                .collect(),
        },
    ))
}

fn arguments(value: Value) -> ToolCallArguments {
    value.as_object().expect("object arguments").clone()
}

/// The `grammarToolInputProperties` fixture: sample_tool -> "payload".
fn grammar_tool_input_properties() -> BTreeMap<String, String> {
    [("sample_tool".to_string(), "payload".to_string())]
        .into_iter()
        .collect()
}

/// `toMatchObject` semantics: objects match partially on the expected keys,
/// everything else compares with deep equality.
fn matches_object(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|found| matches_object(value, found))
        }),
        _ => expected == actual,
    }
}

fn assert_matches_object(expected: &Value, actual: &Value) {
    assert!(
        matches_object(expected, actual),
        "expected {actual} to match {expected}"
    );
}

#[test]
fn converts_supported_constraints_and_falls_back_when_unsupported() {
    // json_schema strict "prefer" with default options converts to a strict
    // function tool.
    let converted = convert_responses_tools(
        &[Tool {
            constrained_sampling: json_schema_config(ConstrainedSamplingStrict::Prefer),
            ..make_tool()
        }],
        None,
    )
    .unwrap();
    assert_matches_object(
        &json!({
            "type": "function",
            "name": "sample_tool",
            "strict": true,
        }),
        &converted[0],
    );

    // strict "require" on a provider without strict mode rejects the tool.
    let error = convert_responses_tools(
        &[Tool {
            constrained_sampling: json_schema_config(ConstrainedSamplingStrict::Require),
            ..make_tool()
        }],
        Some(&ConvertResponsesToolsOptions {
            supports_strict_mode: Some(false),
            ..Default::default()
        }),
    )
    .unwrap_err();
    assert!(
        error.contains(r#"Tool "sample_tool" requires JSON-schema constrained sampling"#),
        "unexpected error: {error}"
    );

    // Grammar constraints convert to custom tools when supported.
    let grammar_tool = Tool {
        constrained_sampling: grammar_config(&[(GrammarFormat::OpenaiLark, "start: /[a-z]+/")]),
        ..make_tool()
    };
    let converted = convert_responses_tools(
        std::slice::from_ref(&grammar_tool),
        Some(&ConvertResponsesToolsOptions {
            supports_openai_grammar_tools: Some(true),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_matches_object(
        &json!({
            "type": "custom",
            "name": "sample_tool",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: /[a-z]+/",
            },
        }),
        &converted[0],
    );

    // Grammar without any supported variant is rejected.
    let error = convert_responses_tools(
        &[Tool {
            constrained_sampling: grammar_config(&[]),
            ..make_tool()
        }],
        Some(&ConvertResponsesToolsOptions {
            supports_openai_grammar_tools: Some(true),
            ..Default::default()
        }),
    )
    .unwrap_err();
    assert!(
        error.contains(
            r#"Tool "sample_tool" cannot use grammar constrained sampling: no supported grammar variant was provided"#
        ),
        "unexpected error: {error}"
    );

    // Unsupported on both axes: falls back to a plain function tool without
    // the strict field.
    let fallback = convert_responses_tools(
        &[grammar_tool],
        Some(&ConvertResponsesToolsOptions {
            supports_openai_grammar_tools: Some(false),
            supports_strict_mode: Some(false),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_matches_object(
        &json!({
            "type": "function",
            "name": "sample_tool",
        }),
        &fallback[0],
    );
    assert!(fallback[0].get("strict").is_none());

    // `constrainedSampling: false` behaves exactly like no configuration.
    assert_eq!(
        convert_responses_tools(
            &[Tool {
                constrained_sampling: Some(ToolConstrainedSampling::Disabled(false)),
                ..make_tool()
            }],
            None,
        )
        .unwrap(),
        convert_responses_tools(&[make_tool()], None).unwrap()
    );
}

#[test]
fn derives_strict_provider_schemas_without_changing_tool_definitions() {
    // Type.Object with two required and three optional properties, where the
    // optional properties include a nested object and a nullable union.
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "offset": { "type": "number" },
            "metadata": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                },
            },
            "nullable": {
                "anyOf": [{ "type": "string" }, { "type": "null" }],
            },
        },
        "required": ["path", "metadata"],
    });

    let strict: Map<String, Value> = make_strict_json_schema(&parameters).unwrap();
    let strict = Value::Object(strict);

    // The original tool definition is unchanged.
    assert_eq!(parameters.get("additionalProperties"), None);
    assert_eq!(
        parameters.get("required"),
        Some(&json!(["path", "metadata"]))
    );

    assert_matches_object(
        &json!({
            "additionalProperties": false,
            "required": ["path", "offset", "metadata", "nullable"],
            "properties": {
                "offset": { "anyOf": [{ "type": "number" }, { "type": "null" }] },
                "metadata": {
                    "additionalProperties": false,
                    "required": ["enabled"],
                    "properties": {
                        "enabled": { "anyOf": [{ "type": "boolean" }, { "type": "null" }] },
                    },
                },
                "nullable": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            },
        }),
        &strict,
    );
}

#[test]
fn falls_back_or_rejects_schemas_that_cannot_be_safely_converted() {
    let cases: Vec<(Value, &str)> = vec![
        (
            // Type.Object({ metadata: Type.Object({}, { additionalProperties: Type.String() }) })
            json!({
                "type": "object",
                "properties": {
                    "metadata": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": { "type": "string" },
                    },
                },
                "required": ["metadata"],
            }),
            "additionalProperties is unsupported",
        ),
        (
            // Type.Intersect([Type.Object({ a: Type.String() }), Type.Object({ b: Type.Number() })])
            json!({
                "allOf": [
                    {
                        "type": "object",
                        "properties": { "a": { "type": "string" } },
                        "required": ["a"],
                    },
                    {
                        "type": "object",
                        "properties": { "b": { "type": "number" } },
                        "required": ["b"],
                    },
                ],
            }),
            "allOf schemas are unsupported",
        ),
        (
            // Type.Object({ value: Type.Union([Type.Object({ nested: Type.String() }), Type.Null()]) })
            json!({
                "type": "object",
                "properties": {
                    "value": {
                        "anyOf": [
                            {
                                "type": "object",
                                "properties": { "nested": { "type": "string" } },
                                "required": ["nested"],
                            },
                            { "type": "null" },
                        ],
                    },
                },
                "required": ["value"],
            }),
            "object and array unions are unsupported",
        ),
        (
            json!({
                "type": "object",
                "properties": {
                    "child": { "$ref": "https://example.com/child.json" },
                },
                "required": ["child"],
            }),
            "$ref schemas are unsupported",
        ),
    ];

    for (parameters, error) in cases {
        let make_tool_with = |strict, parameters: &Value| Tool {
            parameters: parameters.clone(),
            constrained_sampling: json_schema_config(strict),
            ..make_tool()
        };

        // The conversion itself rejects the schema.
        let strict_error = make_strict_json_schema(&parameters).unwrap_err();
        assert!(
            strict_error.contains(error),
            "unexpected error {strict_error} for {parameters}"
        );

        // "prefer" tools fall back to non-strict sampling.
        let tool = make_tool_with(ConstrainedSamplingStrict::Prefer, &parameters);
        assert_eq!(
            resolve_json_schema_strict_sampling(&tool, true).unwrap(),
            None
        );
        let converted = convert_responses_tools(
            std::slice::from_ref(&tool),
            Some(&ConvertResponsesToolsOptions {
                supports_strict_mode: Some(true),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_matches_object(
            &json!({
                "strict": false,
                "parameters": parameters,
            }),
            &converted[0],
        );

        // "require" tools surface the schema error.
        let tool = make_tool_with(ConstrainedSamplingStrict::Require, &parameters);
        let resolve_error = resolve_json_schema_strict_sampling(&tool, true).unwrap_err();
        assert!(
            resolve_error.contains(error),
            "unexpected error: {resolve_error}"
        );
    }
}

/// Port of the `replays grammar calls` context fixture; the tool call arguments
/// are injected per iteration like the TS loop mutates `replayedToolCall`.
fn grammar_replay_context(replayed_arguments: ToolCallArguments) -> Context {
    Context {
        messages: vec![
            Message::Assistant(Box::new(AssistantMessage {
                role: RoleAssistant,
                content: vec![AssistantContent::ToolCall(ToolCall {
                    content_type: Default::default(),
                    id: "call_1|ctc_1".to_string(),
                    name: "sample_tool".to_string(),
                    arguments: replayed_arguments,
                    ..Default::default()
                })],
                api: "openai-responses".to_string(),
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
                usage: make_usage(),
                stop_reason: StopReason::ToolUse,
                timestamp: 0,
                ..Default::default()
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                role: RoleToolResult,
                tool_call_id: "call_1|ctc_1".to_string(),
                tool_name: "sample_tool".to_string(),
                content: vec![BlockContent::Text(TextContent {
                    text: "done".to_string(),
                    ..Default::default()
                })],
                is_error: false,
                timestamp: 0,
                ..Default::default()
            })),
        ],
        ..Default::default()
    }
}

#[test]
fn replays_grammar_calls_as_custom_responses_items() {
    let allowed_tool_call_providers: BTreeSet<String> =
        ["openai".to_string()].into_iter().collect();
    let properties = grammar_tool_input_properties();
    let options = ConvertResponsesMessagesOptions {
        grammar_tool_input_properties: Some(&properties),
        ..Default::default()
    };

    for invalid_arguments in [json!({}), json!({ "payload": 42 })] {
        let error = convert_responses_messages(
            &make_model(),
            &grammar_replay_context(arguments(invalid_arguments)),
            &allowed_tool_call_providers,
            Some(&options),
        )
        .unwrap_err();
        assert!(
            error.contains(
                r#"Grammar tool call "sample_tool" requires argument "payload" to be a string"#
            ),
            "unexpected error: {error}"
        );
    }

    let messages = convert_responses_messages(
        &make_model(),
        &grammar_replay_context(arguments(json!({ "payload": "abc" }))),
        &allowed_tool_call_providers,
        Some(&options),
    )
    .unwrap();

    // `toContainEqual`: deep equality against one converted message.
    assert!(messages.contains(&json!({
        "type": "custom_tool_call",
        "id": "ctc_1",
        "call_id": "call_1",
        "name": "sample_tool",
        "input": "abc",
    })));
    assert!(messages.contains(&json!({
        "type": "custom_tool_call_output",
        "call_id": "call_1",
        "output": "done",
    })));
}

#[test]
fn keeps_grammar_input_json_deltas_append_only() {
    let mut buffer = GrammarToolInputJsonBuffer {
        input: String::new(),
        started: false,
        closed: false,
    };
    let first = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"", false)
        .unwrap()
        .unwrap();
    let second = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true)
        .unwrap()
        .unwrap();

    let joined: Value =
        serde_json::from_str(&format!("{first}{second}")).expect("deltas parse as JSON");
    assert_eq!(joined, json!({ "payload": "a\"\nb" }));

    // Re-closing with the same input is a no-op.
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true).unwrap(),
        None
    );
    // Changing the input after close is an error.
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "payload", "changed", true).unwrap_err(),
        "grammar tool input for property \"payload\" changed after it was closed"
    );
}

#[tokio::test]
async fn streams_custom_responses_tool_calls_as_string_arguments() {
    let mut output = make_output();
    let stream = create_assistant_message_event_stream();
    let events = vec![
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "custom_tool_call", "call_id": "call_1", "id": "ctc_1", "name": "sample_tool", "input": "" },
        }),
        json!({
            "type": "response.custom_tool_call_input.delta",
            "output_index": 0,
            "item_id": "ctc_1",
            "delta": "ab",
        }),
        json!({
            "type": "response.custom_tool_call_input.done",
            "output_index": 0,
            "item_id": "ctc_1",
            "input": "abc",
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "custom_tool_call", "call_id": "call_1", "id": "ctc_1", "name": "sample_tool", "input": "abc" },
        }),
        json!({
            "type": "response.completed",
            "response": { "status": "completed", "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 } },
        }),
    ];

    let options = ResponsesStreamOptions {
        grammar_tool_input_properties: Some(grammar_tool_input_properties()),
        ..Default::default()
    };
    process_responses_stream(
        futures::stream::iter(events.into_iter().map(Ok::<Value, String>)),
        &mut output,
        &stream,
        &make_model(),
        Some(&options),
    )
    .await
    .unwrap();
    stream.end(None);
    let pushed = collect_events(&stream).await;
    // The TS suite wraps `stream.push` to capture toolcall deltas.
    let deltas: Vec<&str> = pushed
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(output.stop_reason, StopReason::ToolUse);
    assert_eq!(output.content.len(), 1);
    match &output.content[0] {
        AssistantContent::ToolCall(tool_call) => {
            assert_eq!(tool_call.id, "call_1|ctc_1");
            assert_eq!(tool_call.name, "sample_tool");
            assert_eq!(
                Value::Object(tool_call.arguments.clone()),
                json!({ "payload": "abc" })
            );
        }
        other => panic!("expected a tool call block, got {other:?}"),
    }
    let joined: Value = serde_json::from_str(&deltas.join("")).expect("deltas parse as JSON");
    assert_eq!(joined, json!({ "payload": "abc" }));
}
