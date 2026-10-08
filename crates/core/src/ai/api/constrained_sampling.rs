//! Port of `pi-core/ai/src/api/constrained-sampling.ts`: strict JSON-schema
//! conversion and grammar-constrained tool sampling configuration.

use serde_json::{Map, Value};

use crate::ai::types::{Tool, ToolCallArguments};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_json_schema_object(value: &Value) -> bool {
    value.is_object()
}

fn is_structured_schema(schema: &Value) -> bool {
    if !is_json_schema_object(schema) {
        return false;
    }
    let object = schema.as_object().unwrap();
    let types: Vec<String> = match object.get("type") {
        Some(Value::String(name)) => vec![name.clone()],
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    types.iter().any(|name| name == "object" || name == "array")
        || object.contains_key("properties")
        || object.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    if !is_json_schema_object(schema) {
        return false;
    }
    let object = schema.as_object().unwrap();
    if object.get("type") == Some(&Value::String("null".to_string()))
        || matches!(object.get("type"), Some(Value::Array(names)) if names.contains(&Value::String("null".to_string())))
    {
        return true;
    }
    if object.get("const") == Some(&Value::Null) {
        return true;
    }
    if let Some(Value::Array(enum_values)) = object.get("enum")
        && enum_values.contains(&Value::Null)
    {
        return true;
    }
    matches!(object.get("anyOf"), Some(Value::Array(variants)) if variants.iter().any(schema_allows_null))
}

fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), String> {
    if !is_json_schema_object(schema) {
        return Err("boolean schemas are unsupported".to_string());
    }
    let object = schema.as_object_mut().unwrap();
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if object.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }

    if let Some(any_of) = object.get_mut("anyOf").map(Value::take) {
        let Value::Array(mut variants) = any_of else {
            return Err("anyOf must contain at least one schema".to_string());
        };
        if variants.is_empty() {
            return Err("anyOf must contain at least one schema".to_string());
        }
        for variant in variants.iter_mut() {
            if is_structured_schema(variant) {
                return Err("object and array unions are unsupported".to_string());
            }
            make_json_schema_node_strict(variant)?;
        }
        object.insert("anyOf".to_string(), Value::Array(variants));
    }

    if let Some(items) = object.get("items").cloned() {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
        make_json_schema_node_strict(object.get_mut("items").unwrap())?;
    }

    let is_object_schema = object.get("type") == Some(&Value::String("object".to_string()));
    if object.contains_key("properties") && !is_object_schema {
        return Err("properties require type object".to_string());
    }
    if !is_object_schema {
        return Ok(());
    }
    if let Some(additional) = object.get("additionalProperties")
        && *additional != Value::Bool(false)
    {
        return Err("schema-valued or true additionalProperties is unsupported".to_string());
    }
    if let Some(properties) = object.get("properties")
        && !properties.is_object()
    {
        return Err("object properties must be a schema map".to_string());
    }
    if let Some(required) = object.get("required")
        && (!required.is_array()
            || required
                .as_array()
                .unwrap()
                .iter()
                .any(|key| !key.is_string()))
    {
        return Err("object required must be a string array".to_string());
    }

    let property_names: Vec<String> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let required: std::collections::BTreeSet<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if required.iter().any(|key| !property_names.contains(key)) {
        return Err("required contains an unknown property".to_string());
    }

    let mut properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, property) in properties.iter_mut() {
        make_json_schema_node_strict(property)?;
        if !required.contains(key) && !schema_allows_null(property) {
            *property = serde_json::json!({"anyOf": [property.take(), {"type": "null"}]});
        }
    }
    object.insert(
        "required".to_string(),
        Value::Array(property_names.iter().cloned().map(Value::String).collect()),
    );
    object.insert("additionalProperties".to_string(), Value::Bool(false));
    if let Some(existing) = object.get_mut("properties") {
        *existing = Value::Object(properties);
    } else {
        object.insert("properties".to_string(), Value::Object(properties));
    }
    Ok(())
}

/// Port of `makeStrictJsonSchema`: converts a tool schema to the strict
/// subset expected by provider constrained sampling. Errors carry the
/// TypeScript messages ("X schemas are unsupported").
pub fn make_strict_json_schema(schema: &Value) -> Result<Map<String, Value>, String> {
    let mut cloned = schema.clone();
    if !is_json_schema_object(&cloned) {
        return Err("root schema must have type object".to_string());
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type") != Some(&Value::String("object".to_string())) {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned.as_object().cloned().unwrap_or_default())
}

/// Port of `getJsonSchemaToolParameters`.
pub fn get_json_schema_tool_parameters(tool: &Tool, strict: bool) -> Result<Value, String> {
    if strict {
        make_strict_json_schema(&tool.parameters).map(Value::Object)
    } else {
        Ok(tool.parameters.clone())
    }
}

/// Port of `GrammarConstrainedSampling`.
#[derive(Clone, Debug, PartialEq)]
pub struct GrammarConstrainedSampling {
    pub format: GrammarFormat,
    pub definition: String,
    pub input_property: String,
}

/// Port of the grammar format values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrammarFormat {
    Lark,
    Regex,
}

/// Port of `GrammarToolInputJsonBuffer`.
#[derive(Clone, Debug, Default)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

/// Port of `getGrammarToolInput`.
pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &ToolCallArguments,
    input_property: &str,
) -> Result<String, String> {
    match arguments.get(input_property) {
        Some(Value::String(input)) => Ok(input.clone()),
        _ => Err(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
        )),
    }
}

/// Port of `appendGrammarToolInputJsonDelta`.
#[allow(clippy::needless_pass_by_ref_mut)]
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, String> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        ));
    }
    if !next_input.starts_with(&buffer.input) {
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        ));
    }

    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        delta.push_str(&format!(
            "{{{}:\"",
            serde_json::to_string(input_property).unwrap_or_default()
        ));
        buffer.started = true;
    }
    let escaped = serde_json::to_string(input_delta).unwrap_or_default();
    delta.push_str(&escaped[1..escaped.len() - 1]);
    buffer.input = next_input.to_string();

    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type") != Some(&Value::String("object".to_string())) {
        return Err("grammar constrained sampling requires an object parameter schema".to_string());
    }
    let required = schema.get("required").and_then(Value::as_array);
    let Some(required) = required else {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    };
    if required.len() != 1 || required[0].as_str().is_none() {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    }

    let input_property = required[0].as_str().unwrap().to_string();
    let property = schema
        .get("properties")
        .and_then(|properties| properties.get(&input_property));
    let Some(property) = property else {
        return Err(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        ));
    };
    if property.get("type") != Some(&Value::String("string".to_string())) {
        return Err(format!(
            "grammar constrained sampling property {input_property} must have type string"
        ));
    }
    Ok(input_property)
}

/// Port of `resolveJsonSchemaStrictSampling`.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(config) = tool.constrained_sampling.as_ref() else {
        return Ok(None);
    };
    let crate::ai::types::ToolConstrainedSampling::Config(
        crate::ai::types::ConstrainedSamplingConfig::JsonSchema { strict },
    ) = config
    else {
        return Ok(None);
    };

    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(error) => {
                if *strict == crate::ai::types::ConstrainedSamplingStrict::Require {
                    return Err(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {error}.",
                        tool.name
                    ));
                }
                Ok(None)
            }
        };
    }
    if *strict == crate::ai::types::ConstrainedSamplingStrict::Require {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// Port of `resolveGrammarConstrainedSampling`.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    let Some(config) = tool.constrained_sampling.as_ref() else {
        return Ok(None);
    };
    let crate::ai::types::ToolConstrainedSampling::Config(
        crate::ai::types::ConstrainedSamplingConfig::Grammar { variants },
    ) = config
    else {
        return Ok(None);
    };

    if !supports_openai_grammar_tools {
        return Ok(None);
    }

    let lark_definition = variants.get(&crate::ai::types::GrammarFormat::OpenaiLark);
    let regex_definition = variants.get(&crate::ai::types::GrammarFormat::OpenaiRegex);
    let has_lark = lark_definition.is_some_and(|definition| !definition.trim().is_empty());
    let has_regex = regex_definition.is_some_and(|definition| !definition.trim().is_empty());
    if !has_lark && !has_regex {
        return Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        ));
    }

    match infer_grammar_input_property(tool) {
        Ok(input_property) => Ok(Some(GrammarConstrainedSampling {
            format: if has_lark {
                GrammarFormat::Lark
            } else {
                GrammarFormat::Regex
            },
            definition: if has_lark {
                lark_definition.cloned().unwrap_or_default()
            } else {
                regex_definition.cloned().unwrap_or_default()
            },
            input_property,
        })),
        Err(message) => Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {message}.",
            tool.name
        )),
    }
}

/// Port of `createGrammarToolInputProperties`.
pub fn create_grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_openai_grammar_tools: bool,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut properties = std::collections::BTreeMap::new();
    for tool in tools.into_iter().flatten() {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}
