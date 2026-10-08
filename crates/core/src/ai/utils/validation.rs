//! Port of `pi-core/ai/src/utils/validation.ts`: tool call argument
//! validation against JSON schemas.
//!
//! TypeScript validates against TypeBox schemas with `Value.Convert` and a
//! custom plain-schema coercion fallback. In Rust every tool schema is a
//! plain JSON schema value, so the port implements the JSON-schema subset the
//! TS pipeline relies on (types, required/properties, items,
//! additionalProperties, allOf/anyOf/oneOf, enum, local `$ref`) and runs the
//! same three phases: optional-null normalization, union-aware coercion, and
//! validation with formatted error paths.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::ai::types::{Tool, ToolCall, ToolCallArguments};

/// The parsed JSON schema node used throughout validation.
type Schema = Value;

/// A schema validation error with its instance path.
#[derive(Clone, Debug, PartialEq)]
struct SchemaError {
    /// Slash-separated path segments relative to the validated root, e.g.
    /// `"/value"` (empty for the root).
    instance_path: String,
    keyword: String,
    message: String,
    /// First missing property for `required` errors.
    required_property: Option<String>,
}

struct Validator<'a> {
    root: &'a Schema,
    /// Memoized `$anchor`-free `$defs`/`definitions` lookup.
    definitions: HashMap<String, &'a Schema>,
}

impl<'a> Validator<'a> {
    fn new(root: &'a Schema) -> Self {
        let mut definitions = HashMap::new();
        if let Some(defs) = root.get("$defs").and_then(Value::as_object) {
            for (name, schema) in defs {
                definitions.insert(name.clone(), schema);
            }
        }
        if let Some(defs) = root.get("definitions").and_then(Value::as_object) {
            for (name, schema) in defs {
                definitions.entry(name.clone()).or_insert(schema);
            }
        }
        Self { root, definitions }
    }

    fn resolve_ref(&self, reference: &str) -> Option<&'a Schema> {
        let reference = reference.strip_prefix('#')?;
        if reference.is_empty() {
            return Some(self.root);
        }
        let path = reference.trim_start_matches('/');
        let def_key = path
            .strip_prefix("$defs/")
            .or_else(|| path.strip_prefix("definitions/"))
            .filter(|rest| !rest.contains('/'));
        if let Some(key) = def_key
            && let Some(schema) = self.definitions.get(key)
        {
            return Some(schema);
        }
        let mut current = self.root;
        for segment in path.split('/') {
            let segment = segment.replace("~1", "/").replace("~0", "~");
            current = current.get(&segment)?;
        }
        Some(current)
    }

    /// Checks a value against a schema, collecting all errors.
    fn check(&self, value: &Value, schema: &Schema, path: &str) -> Vec<SchemaError> {
        let mut errors = Vec::new();
        self.check_node(value, schema, path, &mut errors);
        errors
    }

    fn fail(&self, errors: &mut Vec<SchemaError>, path: &str, keyword: &str, message: String) {
        errors.push(SchemaError {
            instance_path: path.to_string(),
            keyword: keyword.to_string(),
            message,
            required_property: None,
        });
    }

    fn check_node(
        &self,
        value: &Value,
        schema: &Schema,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        // $ref dispatch.
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            if let Some(resolved) = self.resolve_ref(reference) {
                self.check_node(value, resolved, path, errors);
            } else {
                self.fail(
                    errors,
                    path,
                    "$ref",
                    format!("unresolvable $ref {reference}"),
                );
            }
            return;
        }

        // Boolean schemas.
        if let Some(enabled) = schema.as_bool() {
            if !enabled {
                self.fail(errors, path, "schema", "schema is false".to_string());
            }
            return;
        }

        let Some(object) = schema.as_object() else {
            return;
        };

        // allOf / anyOf / oneOf
        if let Some(all_of) = object.get("allOf").and_then(Value::as_array) {
            for sub in all_of {
                self.check_node(value, sub, path, errors);
            }
        }
        if let Some(any_of) = object.get("anyOf").and_then(Value::as_array)
            && !any_of.iter().any(|sub| self.check_no_errors(value, sub))
        {
            self.fail(
                errors,
                path,
                "anyOf",
                "value does not match any schema".to_string(),
            );
        }
        if let Some(one_of) = object.get("oneOf").and_then(Value::as_array) {
            let matches = one_of
                .iter()
                .filter(|sub| self.check_no_errors(value, sub))
                .count();
            if matches != 1 {
                self.fail(
                    errors,
                    path,
                    "oneOf",
                    format!("value matches {matches} schemas, expected exactly one"),
                );
            }
        }

        // enum
        if let Some(enum_values) = object.get("enum").and_then(Value::as_array)
            && !enum_values.contains(value)
        {
            self.fail(
                errors,
                path,
                "enum",
                "value is not one of the allowed values".to_string(),
            );
        }

        // type (string or array)
        let type_names: Vec<String> = match object.get("type") {
            Some(Value::String(name)) => vec![name.clone()],
            Some(Value::Array(names)) => names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        if !type_names.is_empty() && !type_names.iter().any(|name| matches_json_type(value, name)) {
            self.fail(
                errors,
                path,
                "type",
                format!(
                    "expected {}",
                    type_names
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" or ")
                ),
            );
            // Nested structure checks are meaningless when the type already
            // mismatches.
            return;
        }

        if matches_json_type(value, "object") {
            self.check_object(value, object, path, errors);
        }
        if matches_json_type(value, "array") {
            self.check_array(value, object, path, errors);
        }
    }

    fn check_no_errors(&self, value: &Value, schema: &Schema) -> bool {
        let mut errors = Vec::new();
        self.check_node(value, schema, "", &mut errors);
        errors.is_empty()
    }

    fn check_object(
        &self,
        value: &Value,
        object: &Map<String, Value>,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        let Some(map) = value.as_object() else {
            return;
        };

        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                if let Some(property_value) = map.get(name) {
                    let property_path = format!("{path}/{name}");
                    self.check_node(property_value, property_schema, &property_path, errors);
                }
            }
        }

        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    errors.push(SchemaError {
                        instance_path: path.to_string(),
                        keyword: "required".to_string(),
                        message: format!("required property '{name}' is missing"),
                        required_property: Some(name.to_string()),
                    });
                }
            }
        }

        match object.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                let defined: Vec<&String> = object
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().collect())
                    .unwrap_or_default();
                for key in map.keys() {
                    if !defined.iter().any(|name| name.as_str() == key) {
                        self.fail(
                            errors,
                            &format!("{path}/{key}"),
                            "additionalProperties",
                            "additional property is not allowed".to_string(),
                        );
                    }
                }
            }
            Some(additional @ Value::Object(_)) => {
                let defined: Vec<&String> = object
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().collect())
                    .unwrap_or_default();
                for (key, property_value) in map {
                    if !defined.iter().any(|name| name.as_str() == key) {
                        let property_path = format!("{path}/{key}");
                        self.check_node(property_value, additional, &property_path, errors);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_array(
        &self,
        value: &Value,
        object: &Map<String, Value>,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        let Some(items) = value.as_array() else {
            return;
        };
        match object.get("items") {
            Some(schema @ Value::Object(_)) => {
                for (index, item) in items.iter().enumerate() {
                    let item_path = format!("{path}/{index}");
                    self.check_node(item, schema, &item_path, errors);
                }
            }
            Some(schemas @ Value::Array(_)) => {
                for (index, item) in items.iter().enumerate() {
                    if let Some(schema) = schemas.get(index) {
                        let item_path = format!("{path}/{index}");
                        self.check_node(item, schema, &item_path, errors);
                    }
                }
            }
            _ => {}
        }
    }
}

fn matches_json_type(value: &Value, type_name: &str) -> bool {
    match type_name {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn get_schema_types(schema: &Schema) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(name)) => vec![name.clone()],
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn coerce_primitive_by_type(value: &Value, type_name: &str) -> Value {
    match type_name {
        "number" => {
            if value.is_null() {
                return Value::from(0);
            }
            if let Some(text) = value.as_str()
                && !text.trim().is_empty()
                && let Ok(parsed) = text.parse::<f64>()
                && parsed.is_finite()
            {
                return json_number(parsed);
            }
            if let Some(flag) = value.as_bool() {
                return Value::from(if flag { 1 } else { 0 });
            }
            value.clone()
        }
        "integer" => {
            if value.is_null() {
                return Value::from(0);
            }
            if let Some(text) = value.as_str()
                && !text.trim().is_empty()
                && let Ok(parsed) = text.parse::<i64>()
            {
                return Value::from(parsed);
            }
            if let Some(flag) = value.as_bool() {
                return Value::from(if flag { 1 } else { 0 });
            }
            value.clone()
        }
        "boolean" => {
            if value.is_null() {
                return Value::Bool(false);
            }
            if let Some(text) = value.as_str() {
                if text == "true" {
                    return Value::Bool(true);
                }
                if text == "false" {
                    return Value::Bool(false);
                }
            }
            if let Some(number) = value.as_f64() {
                if number == 1.0 {
                    return Value::Bool(true);
                }
                if number == 0.0 {
                    return Value::Bool(false);
                }
            }
            value.clone()
        }
        "string" => {
            if value.is_null() {
                return Value::String(String::new());
            }
            if let Some(number) = value.as_f64() {
                return Value::String(js_number_to_string(number));
            }
            if let Some(flag) = value.as_bool() {
                return Value::String(flag.to_string());
            }
            value.clone()
        }
        "null" => {
            if value == &Value::String(String::new())
                || value.as_f64() == Some(0.0)
                || value == &Value::Bool(false)
            {
                return Value::Null;
            }
            value.clone()
        }
        _ => value.clone(),
    }
}

/// Formats an integral f64 as an integer JSON number, matching JS semantics.
fn json_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Renders a JS number the way `String(value)` does.
fn js_number_to_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn coerce_with_union_schema(value: &Value, schemas: &[Value]) -> Value {
    for schema in schemas {
        if check_value(value, schema) {
            return value.clone();
        }
    }
    for schema in schemas {
        let coerced = coerce_with_json_schema(value, schema);
        if check_value(&coerced, schema) {
            return coerced;
        }
    }
    value.clone()
}

fn check_value(value: &Value, schema: &Schema) -> bool {
    let validator = Validator::new(schema);
    validator.check_no_errors(value, schema)
}

fn coerce_with_json_schema(value: &Value, schema: &Schema) -> Value {
    let mut next_value = value.clone();

    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            next_value = coerce_with_json_schema(&next_value, nested);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
        next_value = coerce_with_union_schema(&next_value, any_of);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        next_value = coerce_with_union_schema(&next_value, one_of);
    }

    let schema_types = get_schema_types(schema);
    let matches_union_member = schema_types.len() > 1
        && schema_types
            .iter()
            .any(|schema_type| matches_json_type(&next_value, schema_type));
    if !schema_types.is_empty() && !matches_union_member {
        for schema_type in &schema_types {
            let candidate = coerce_primitive_by_type(&next_value, schema_type);
            if candidate != next_value {
                next_value = candidate;
                break;
            }
        }
    }

    if schema_types.iter().any(|name| name == "object") && next_value.is_object() {
        apply_schema_object_coercion(&mut next_value, schema);
    }
    if schema_types.iter().any(|name| name == "array") && next_value.is_array() {
        apply_schema_array_coercion(&mut next_value, schema);
    }

    next_value
}

fn apply_schema_object_coercion(value: &mut Value, schema: &Schema) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    let defined_keys: Vec<String> = properties
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();

    if let Some(properties) = properties {
        for (key, property_schema) in properties {
            if let Some(property_value) = map.get_mut(key) {
                *property_value = coerce_with_json_schema(property_value, property_schema);
            }
        }
    }

    if let Some(additional) = schema.get("additionalProperties")
        && additional.is_object()
    {
        for (key, property_value) in map.iter_mut() {
            if !defined_keys.contains(key) {
                *property_value = coerce_with_json_schema(property_value, additional);
            }
        }
    }
}

fn apply_schema_array_coercion(value: &mut Value, schema: &Schema) {
    let Some(items) = value.as_array_mut() else {
        return;
    };
    match schema.get("items") {
        Some(schemas @ Value::Array(_)) => {
            for (index, item) in items.iter_mut().enumerate() {
                if let Some(item_schema) = schemas.get(index) {
                    *item = coerce_with_json_schema(item, item_schema);
                }
            }
        }
        Some(item_schema @ Value::Object(_)) => {
            for item in items.iter_mut() {
                *item = coerce_with_json_schema(item, item_schema);
            }
        }
        _ => {}
    }
}

fn normalize_optional_nulls(value: &mut Value, schema: &Schema) {
    if let Some(items) = value.as_array_mut() {
        match schema.get("items") {
            Some(schemas @ Value::Array(_)) => {
                for (index, item) in items.iter_mut().enumerate() {
                    if let Some(item_schema) = schemas.get(index) {
                        normalize_optional_nulls(item, item_schema);
                    }
                }
            }
            Some(item_schema) => {
                for item in items.iter_mut() {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            None => {}
        }
        return;
    }
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required: Vec<String> = schema
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

    for (key, property_schema) in properties {
        let Some(property_value) = map.get_mut(key) else {
            continue;
        };
        let is_ref = property_schema
            .get("$ref")
            .and_then(Value::as_str)
            .is_some();
        let accepts_null = check_value(&Value::Null, property_schema);
        if property_value.is_null() && !required.contains(key) && !is_ref && !accepts_null {
            map.remove(key);
        } else {
            normalize_optional_nulls(property_value, property_schema);
        }
    }
}

/// Formats a validation path the way `formatValidationPath` does.
fn format_validation_path(error: &SchemaError) -> String {
    let path = error
        .instance_path
        .trim_start_matches('/')
        .replace('/', ".");
    if error.keyword == "required"
        && let Some(required_property) = &error.required_property
    {
        return if path.is_empty() {
            required_property.clone()
        } else {
            format!("{path}.{required_property}")
        };
    }
    if path.is_empty() {
        "root".to_string()
    } else {
        path
    }
}

/// Port of `validateToolCall`: finds a tool by name and validates the tool
/// call arguments against its schema.
pub fn validate_tool_call(
    tools: &[Tool],
    tool_call: &ToolCall,
) -> Result<ToolCallArguments, String> {
    let tool = tools
        .iter()
        .find(|tool| tool.name == tool_call.name)
        .ok_or_else(|| format!("Tool \"{}\" not found", tool_call.name))?;
    validate_tool_arguments(tool, tool_call)
}

/// Port of `validateToolArguments`: validates (and potentially coerces) tool
/// call arguments against the tool's JSON schema.
pub fn validate_tool_arguments(
    tool: &Tool,
    tool_call: &ToolCall,
) -> Result<ToolCallArguments, String> {
    let schema = &tool.parameters;
    let mut args = Value::Object(tool_call.arguments.clone());
    normalize_optional_nulls(&mut args, schema);
    let args = coerce_with_json_schema(&args, schema);

    let validator = Validator::new(schema);
    let errors = validator.check(&args, schema, "");
    if errors.is_empty() {
        return args
            .as_object()
            .cloned()
            .ok_or_else(|| "validated arguments are not an object".to_string());
    }

    let formatted = errors
        .iter()
        .map(|error| format!("  - {}: {}", format_validation_path(error), error.message))
        .collect::<Vec<_>>()
        .join("\n");
    let formatted = if formatted.is_empty() {
        "Unknown validation error".to_string()
    } else {
        formatted
    };
    let received = serde_json::to_string_pretty(&tool_call.arguments).unwrap_or_default();
    Err(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
        tool_call.name, formatted, received
    ))
}
