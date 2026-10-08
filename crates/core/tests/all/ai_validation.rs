//! Port of `pi-core/ai/test/validation.test.ts`.

use serde_json::{Value, json};

use pi_core::ai::types::{Tool, ToolCall};
use pi_core::ai::utils::validation::validate_tool_arguments;

fn create_tool_call_with_plain_schema(schema: Value, value: Value) -> (Tool, ToolCall) {
    let tool = Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "value": schema,
            },
            "required": ["value"],
        }),
        constrained_sampling: None,
    };
    let tool_call = ToolCall {
        content_type: Default::default(),
        id: "tool-1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::from_value(json!({ "value": value })).unwrap(),
        ..Default::default()
    };
    (tool, tool_call)
}

#[test]
fn coerces_serialized_plain_json_schemas_with_ajv_compatible_primitive_rules() {
    let passing_cases: Vec<(Value, Value, Value)> = vec![
        (json!({"type": "number"}), json!("42"), json!(42)),
        (json!({"type": "number"}), json!(true), json!(1)),
        (json!({"type": "number"}), json!(null), json!(0)),
        (json!({"type": "integer"}), json!("42"), json!(42)),
        (json!({"type": "boolean"}), json!("true"), json!(true)),
        (json!({"type": "boolean"}), json!("false"), json!(false)),
        (json!({"type": "boolean"}), json!(1), json!(true)),
        (json!({"type": "boolean"}), json!(0), json!(false)),
        (json!({"type": "string"}), json!(null), json!("")),
        (json!({"type": "string"}), json!(true), json!("true")),
        (json!({"type": "null"}), json!(""), json!(null)),
        (json!({"type": "null"}), json!(0), json!(null)),
        (json!({"type": "null"}), json!(false), json!(null)),
        (
            json!({"type": ["number", "string"]}),
            json!("1"),
            json!("1"),
        ),
        (json!({"type": ["boolean", "number"]}), json!("1"), json!(1)),
    ];

    for (schema, input, expected) in passing_cases {
        let display = input.to_string();
        let (tool, tool_call) = create_tool_call_with_plain_schema(schema, input.clone());
        let result = validate_tool_arguments(&tool, &tool_call)
            .unwrap_or_else(|error| panic!("validation failed for {display}: {error}"));
        assert_eq!(
            result,
            serde_json::from_value::<serde_json::Map<String, Value>>(json!({ "value": expected }))
                .unwrap(),
            "input {display}"
        );
    }
}

#[test]
fn treats_null_as_omission_for_optional_non_nullable_properties() {
    let tool = Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "number"},
                "nullable": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "metadata": {
                    "type": "object",
                    "properties": {"enabled": {"type": "boolean"}},
                },
            },
        }),
        constrained_sampling: None,
    };
    let tool_call = ToolCall {
        content_type: Default::default(),
        id: "tool-1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::from_value(json!({
            "path": "file.txt",
            "offset": null,
            "nullable": null,
            "metadata": {"enabled": null},
        }))
        .unwrap(),
        ..Default::default()
    };

    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "path": "file.txt",
            "nullable": null,
            "metadata": {},
        }))
        .unwrap()
    );
}

#[test]
fn preserves_optional_nulls_whose_referenced_schema_is_nullable() {
    let tool = Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {"value": {"$ref": "#/$defs/value"}},
            "$defs": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}},
        }),
        constrained_sampling: None,
    };
    let tool_call = ToolCall {
        content_type: Default::default(),
        id: "tool-1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::from_value(json!({"value": null})).unwrap(),
        ..Default::default()
    };

    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value(json!({"value": null})).unwrap()
    );
}

#[test]
fn preserves_a_value_that_already_matches_a_nullable_union_arm() {
    let tool = Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "value": {"anyOf": [{"type": "number"}, {"type": "null"}]},
            },
        }),
        constrained_sampling: None,
    };
    let tool_call = ToolCall {
        content_type: Default::default(),
        id: "tool-1".to_string(),
        name: "echo".to_string(),
        arguments: serde_json::from_value(json!({"value": null})).unwrap(),
        ..Default::default()
    };

    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value(json!({"value": null})).unwrap()
    );
}

#[test]
fn preserves_a_value_that_already_matches_a_oneof_nullable_union_arm() {
    let (tool, tool_call) = create_tool_call_with_plain_schema(
        json!({"oneOf": [{"type": "number"}, {"type": "null"}]}),
        json!(null),
    );
    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value(json!({"value": null})).unwrap()
    );
}

#[test]
fn still_coerces_nullable_unions_when_the_original_value_does_not_match_any_arm() {
    let (tool, tool_call) = create_tool_call_with_plain_schema(
        json!({"anyOf": [{"type": "number"}, {"type": "null"}]}),
        json!("42"),
    );
    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value(json!({"value": 42})).unwrap()
    );
}

#[test]
fn accepts_null_for_nullable_array_schemas_with_items() {
    let (tool, tool_call) = create_tool_call_with_plain_schema(
        json!({"type": ["array", "null"], "items": {"type": "string"}}),
        json!(null),
    );
    let result = validate_tool_arguments(&tool, &tool_call).expect("validates");
    assert_eq!(
        result,
        serde_json::from_value(json!({"value": null})).unwrap()
    );
}

#[test]
fn rejects_invalid_coercions_for_serialized_plain_json_schemas() {
    let failing_cases: Vec<(Value, Value)> = vec![
        (json!({"type": "boolean"}), json!("1")),
        (json!({"type": "boolean"}), json!("0")),
        (json!({"type": "null"}), json!("null")),
        (json!({"type": "integer"}), json!("42.1")),
    ];

    for (schema, input) in failing_cases {
        let display = input.to_string();
        let (tool, tool_call) = create_tool_call_with_plain_schema(schema, input.clone());
        let error = validate_tool_arguments(&tool, &tool_call).unwrap_err();
        assert!(
            error.starts_with("Validation failed"),
            "unexpected error for {display}: {error}"
        );
    }
}

#[test]
fn validate_tool_call_reports_missing_tools() {
    use pi_core::ai::utils::validation::validate_tool_call;
    let tools = vec![Tool {
        name: "echo".to_string(),
        description: "Echo".to_string(),
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
    }];
    let tool_call = ToolCall {
        content_type: Default::default(),
        id: "tool-1".to_string(),
        name: "missing".to_string(),
        arguments: Default::default(),
        ..Default::default()
    };
    let error = validate_tool_call(&tools, &tool_call).unwrap_err();
    assert_eq!(error, "Tool \"missing\" not found");
}
