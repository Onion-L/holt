//! Port of `pi-core/agent/src/harness/session/jsonl/codec.ts`.

use serde_json::{Map, Value};

use super::super::state::SessionMutation;
use super::super::types::{Entry, LaneRecord};
use super::errors::JsonlDecodeError;
use super::types::{JsonlSessionMetadata, JsonlV4Header};

const ENTRY_TYPES: &[&str] = &[
    "message",
    "model_change",
    "thinking_level_change",
    "active_tools_change",
    "compaction",
    "branch_summary",
    "custom",
];
const RECORD_TYPES: &[&str] = &[
    "operation_started",
    "abort_requested",
    "operation_finished",
    "step_attempt",
    "tool_started",
    "queue_enqueued",
    "queue_cancelled",
    "write_deferred",
    "usage",
];
const OPERATION_KINDS: &[&str] = &["run", "compaction", "navigation"];

fn parse_object(line: &str) -> Result<Map<String, Value>, JsonlDecodeError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| JsonlDecodeError::syntax(format!("is not valid JSON: {error}")))?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(JsonlDecodeError::schema("is not a JSON object")),
    }
}

fn require_string(value: Option<&Value>, field: &str) -> Result<String, JsonlDecodeError> {
    value
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| JsonlDecodeError::schema(format!("has invalid {field}")))
}

fn require_sequence(value: Option<&Value>) -> Result<u64, JsonlDecodeError> {
    let Some(number) = value.and_then(|value| value.as_u64()) else {
        return Err(JsonlDecodeError::schema("has invalid seq"));
    };
    if number == 0 || number > i64::MAX as u64 {
        return Err(JsonlDecodeError::schema("has invalid seq"));
    }
    Ok(number)
}

fn require_timestamp(value: Option<&Value>) -> Result<i64, JsonlDecodeError> {
    let Some(number) = value.and_then(|value| value.as_i64()) else {
        return Err(JsonlDecodeError::schema("has invalid timestamp"));
    };
    if number < 0 {
        return Err(JsonlDecodeError::schema("has invalid timestamp"));
    }
    Ok(number)
}

fn require_nullable_id(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<String>, JsonlDecodeError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(JsonlDecodeError::schema(format!("has invalid {field}"))),
    }
}

fn decode_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    let value = parse_object(line)?;
    if value.get("kind").and_then(|kind| kind.as_str()) != Some("header") {
        return Err(JsonlDecodeError::schema("is not a header"));
    }
    if value.get("version").and_then(|version| version.as_u64()) != Some(4) {
        return Err(JsonlDecodeError::schema("has unsupported session version"));
    }
    // TypeScript treats an explicit null like any other non-string value.
    let parent_session_id = match value.get("parentSessionId") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(JsonlDecodeError::schema("has invalid parentSessionId")),
    };
    let legacy_parent_session_path = match value.get("legacyParentSessionPath") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(JsonlDecodeError::schema(
                "has invalid legacyParentSessionPath",
            ));
        }
    };
    if parent_session_id.is_some() && legacy_parent_session_path.is_some() {
        return Err(JsonlDecodeError::schema(
            "has both parentSessionId and legacyParentSessionPath",
        ));
    }
    let metadata = match value.get("metadata") {
        None => None,
        Some(metadata @ Value::Object(_)) => Some(metadata.clone()),
        Some(_) => return Err(JsonlDecodeError::schema("has invalid metadata")),
    };
    Ok(JsonlV4Header {
        version: 4,
        id: require_string(value.get("id"), "id")?,
        created_at: require_timestamp(value.get("createdAt"))?,
        cwd: require_string(value.get("cwd"), "cwd")?,
        parent_session_id,
        legacy_parent_session_path,
        metadata,
    })
}

/// Port of `parseHeader`.
pub fn parse_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    decode_header(line)
}

/// Port of `encodeHeader` (trailing newline included).
pub fn encode_header(header: &JsonlV4Header) -> String {
    let mut object = Map::new();
    object.insert("kind".to_string(), Value::String("header".to_string()));
    object.insert("version".to_string(), Value::from(4));
    object.insert("id".to_string(), Value::String(header.id.clone()));
    object.insert("createdAt".to_string(), Value::from(header.created_at));
    object.insert("cwd".to_string(), Value::String(header.cwd.clone()));
    if let Some(parent) = &header.parent_session_id {
        object.insert("parentSessionId".to_string(), Value::String(parent.clone()));
    }
    if let Some(legacy) = &header.legacy_parent_session_path {
        object.insert(
            "legacyParentSessionPath".to_string(),
            Value::String(legacy.clone()),
        );
    }
    if let Some(metadata) = &header.metadata {
        object.insert("metadata".to_string(), metadata.clone());
    }
    format!("{}\n", Value::Object(object))
}

/// Port of `metadataFromHeader`.
pub fn metadata_from_header(
    header: &JsonlV4Header,
    path: impl Into<String>,
    modified_at: f64,
) -> JsonlSessionMetadata {
    JsonlSessionMetadata {
        id: header.id.clone(),
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        path: path.into(),
        modified_at,
        source_format: 4,
        parent_session_id: header.parent_session_id.clone(),
        legacy_parent_session_path: header.legacy_parent_session_path.clone(),
        metadata: header.metadata.clone(),
    }
}

fn parse_entry_mutation(
    value: &Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    let lane = match value.get("lane") {
        None => None,
        Some(Value::String(lane)) => Some(lane.clone()),
        Some(_) => return Err(JsonlDecodeError::schema("has invalid lane")),
    };
    let id = require_string(value.get("id"), "id")?;
    let entry_type = require_string(value.get("type"), "entry type")?;
    if !ENTRY_TYPES.contains(&entry_type.as_str()) {
        return Err(JsonlDecodeError::schema(format!(
            "has unknown entry type {entry_type}"
        )));
    }
    let parent_id = require_nullable_id(value.get("parentId"), "parentId")?;
    let timestamp = require_timestamp(value.get("timestamp"))?;
    if entry_type == "custom" {
        require_string(value.get("customType"), "customType")?;
    }
    // Rebuild the entry payload without the mutation envelope fields, in
    // the wire's own field order.
    let mut entry_object = Map::new();
    for (key, field) in value {
        if key == "kind" || key == "lane" {
            continue;
        }
        entry_object.insert(key.clone(), field.clone());
    }
    entry_object.insert("id".to_string(), Value::String(id));
    entry_object.insert("seq".to_string(), Value::from(seq));
    if parent_id.is_none() {
        entry_object.insert("parentId".to_string(), Value::Null);
    }
    entry_object.insert("timestamp".to_string(), Value::from(timestamp));
    let entry: Entry = serde_json::from_value(Value::Object(entry_object))
        .map_err(|error| JsonlDecodeError::schema(format!("has invalid entry: {error}")))?;
    Ok(SessionMutation::Entry { lane, entry })
}

fn parse_record_mutation(
    value: &Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    let id = require_string(value.get("id"), "id")?;
    let lane = require_string(value.get("lane"), "lane")?;
    let record_type = require_string(value.get("type"), "record type")?;
    if !RECORD_TYPES.contains(&record_type.as_str()) {
        return Err(JsonlDecodeError::schema(format!(
            "has unknown record type {record_type}"
        )));
    }
    require_timestamp(value.get("timestamp"))?;
    if record_type == "operation_started" {
        let Some(Value::Object(intent)) = value.get("intent") else {
            return Err(JsonlDecodeError::schema("has invalid intent"));
        };
        let operation_kind = require_string(intent.get("kind"), "operation kind")?;
        if !OPERATION_KINDS.contains(&operation_kind.as_str()) {
            return Err(JsonlDecodeError::schema(format!(
                "has unknown operation kind {operation_kind}"
            )));
        }
    }
    if record_type == "operation_finished" {
        require_string(value.get("runId"), "runId")?;
    }
    let mut record_object = Map::new();
    for (key, field) in value {
        if key == "kind" {
            continue;
        }
        record_object.insert(key.clone(), field.clone());
    }
    record_object.insert("id".to_string(), Value::String(id));
    record_object.insert("lane".to_string(), Value::String(lane));
    record_object.insert("seq".to_string(), Value::from(seq));
    let record: LaneRecord = serde_json::from_value(Value::Object(record_object))
        .map_err(|error| JsonlDecodeError::schema(format!("has invalid record: {error}")))?;
    Ok(SessionMutation::Record { record })
}

fn parse_lane_mutation(
    value: &Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    Ok(SessionMutation::Lane {
        seq,
        lane: require_string(value.get("lane"), "lane")?,
        leaf_id: require_nullable_id(value.get("leafId"), "leafId")?,
    })
}

fn parse_fact_mutation(
    value: &Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    match value.get("fact").and_then(|fact| fact.as_str()) {
        Some("name") => {
            let name = match value.get("name") {
                None => None,
                Some(Value::String(name)) => Some(name.clone()),
                Some(_) => return Err(JsonlDecodeError::schema("has invalid name")),
            };
            Ok(SessionMutation::NameFact { seq, name })
        }
        Some("label") => {
            let label = match value.get("label") {
                None => None,
                Some(Value::String(label)) => Some(label.clone()),
                Some(_) => return Err(JsonlDecodeError::schema("has invalid label")),
            };
            Ok(SessionMutation::LabelFact {
                seq,
                target_id: require_string(value.get("targetId"), "targetId")?,
                label,
            })
        }
        _ => Err(JsonlDecodeError::schema("has unknown fact type")),
    }
}

fn decode_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    let value = parse_object(line)?;
    let seq = require_sequence(value.get("seq"))?;
    match value.get("kind").and_then(|kind| kind.as_str()) {
        Some("entry") => parse_entry_mutation(&value, seq),
        Some("record") => parse_record_mutation(&value, seq),
        Some("lane") => parse_lane_mutation(&value, seq),
        Some("fact") => parse_fact_mutation(&value, seq),
        _ => Err(JsonlDecodeError::schema("has unknown mutation kind")),
    }
}

/// Port of `parseMutation`.
pub fn parse_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    decode_mutation(line)
}

/// Port of `encodeMutation` (trailing newline included).
pub fn encode_mutation(mutation: &SessionMutation) -> String {
    match mutation {
        SessionMutation::Entry { lane, entry } => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String("entry".to_string()));
            if let Some(lane) = lane {
                object.insert("lane".to_string(), Value::String(lane.clone()));
            }
            if let Value::Object(entry_object) =
                serde_json::to_value(entry).expect("entries are JSON-serializable")
            {
                for (key, field) in entry_object {
                    object.insert(key, field);
                }
            }
            format!("{}\n", Value::Object(object))
        }
        SessionMutation::Record { record } => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String("record".to_string()));
            if let Value::Object(record_object) =
                serde_json::to_value(record).expect("records are JSON-serializable")
            {
                for (key, field) in record_object {
                    object.insert(key, field);
                }
            }
            format!("{}\n", Value::Object(object))
        }
        SessionMutation::Lane { seq, lane, leaf_id } => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String("lane".to_string()));
            object.insert("seq".to_string(), Value::from(*seq));
            object.insert("lane".to_string(), Value::String(lane.clone()));
            match leaf_id {
                Some(leaf_id) => {
                    object.insert("leafId".to_string(), Value::String(leaf_id.clone()));
                }
                None => {
                    object.insert("leafId".to_string(), Value::Null);
                }
            }
            format!("{}\n", Value::Object(object))
        }
        SessionMutation::NameFact { seq, name } => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String("fact".to_string()));
            object.insert("seq".to_string(), Value::from(*seq));
            object.insert("fact".to_string(), Value::String("name".to_string()));
            if let Some(name) = name {
                object.insert("name".to_string(), Value::String(name.clone()));
            }
            format!("{}\n", Value::Object(object))
        }
        SessionMutation::LabelFact {
            seq,
            target_id,
            label,
        } => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String("fact".to_string()));
            object.insert("seq".to_string(), Value::from(*seq));
            object.insert("fact".to_string(), Value::String("label".to_string()));
            object.insert("targetId".to_string(), Value::String(target_id.clone()));
            if let Some(label) = label {
                object.insert("label".to_string(), Value::String(label.clone()));
            }
            format!("{}\n", Value::Object(object))
        }
    }
}
