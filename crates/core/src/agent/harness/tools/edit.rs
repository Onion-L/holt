//! Port of `pi-core/agent/src/harness/tools/edit.ts`.

use std::sync::Arc;

use crate::agent::harness::types::{AgentToolResult, WriteContent};
use crate::ai::types::{BlockContent, TextContent};

use super::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom,
};
use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_tool_path;
use super::tool_context::as_execution_tool_context;

/// Port of `EditToolInput`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EditToolInput {
    pub path: String,
    pub edits: Vec<super::edit_diff::Edit>,
}

/// Port of `EditToolDetails`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_changed_line: Option<usize>,
}

pub fn edit_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["path", "edits"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                "items": {
                    "type": "object",
                    "required": ["oldText", "newText"],
                    "properties": {
                        "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                        "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                    }
                }
            }
        }
    })
}
fn edit_access_error(path: &str, error: &crate::agent::harness::types::FileError) -> String {
    format!(
        "Could not edit file: {path}. Error code: {}.",
        code_name(error.code)
    )
}

fn code_name(code: crate::agent::harness::types::FileErrorCode) -> &'static str {
    use crate::agent::harness::types::FileErrorCode::*;
    match code {
        Aborted => "aborted",
        NotFound => "not_found",
        PermissionDenied => "permission_denied",
        NotDirectory => "not_directory",
        IsDirectory => "is_directory",
        Invalid => "invalid",
        NotSupported => "not_supported",
        Unknown => "unknown",
    }
}

fn is_single_edit_input(value: &serde_json::Value) -> bool {
    value.get("oldText").and_then(|v| v.as_str()).is_some()
        && value.get("newText").and_then(|v| v.as_str()).is_some()
}

/// Port of `prepareEditArguments` (JSON-string edits, single-edit form,
/// and the legacy top-level oldText/newText keys).
pub fn prepare_edit_arguments(input: &serde_json::Value) -> serde_json::Value {
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut args = object.clone();

    if let Some(edits) = args.get("edits") {
        if let Some(text) = edits.as_str() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
                if parsed.is_array() {
                    args.insert("edits".to_string(), parsed);
                } else if is_single_edit_input(&parsed) {
                    args.insert("edits".to_string(), serde_json::json!([parsed]));
                }
            }
        } else if is_single_edit_input(edits) {
            args.insert("edits".to_string(), serde_json::json!([edits]));
        }
    }

    let legacy_old = args.get("oldText").and_then(|value| value.as_str());
    let legacy_new = args.get("newText").and_then(|value| value.as_str());
    if let (Some(old_text), Some(new_text)) = (legacy_old, legacy_new) {
        let mut edits = args
            .get("edits")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        edits.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
        args.remove("oldText");
        args.remove("newText");
        args.insert("edits".to_string(), serde_json::Value::Array(edits));
    }
    serde_json::Value::Object(args)
}

fn parse_edits(input: &serde_json::Value) -> Result<(String, Vec<Edit>), String> {
    let path = input
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let Some(edits) = input.get("edits").and_then(|value| value.as_array()) else {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    };
    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    }
    let parsed = edits
        .iter()
        .map(|edit| Edit {
            old_text: edit
                .get("oldText")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            new_text: edit
                .get("newText")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
        })
        .collect();
    Ok((path, parsed))
}

/// Port of `createEditTool`.
pub fn create_edit_tool() -> crate::agent::harness::types::AgentHarnessTool {
    crate::agent::harness::types::AgentHarnessTool {
        name: "edit".to_string(),
        label: "edit".to_string(),
        description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_string(),
        parameters: edit_schema(),
        constrained_sampling: None,
        prepare_arguments: Some(Arc::new(prepare_edit_arguments)),
        execution_mode: None,
        replay: None,
        execute: Arc::new(
            |_tool_call_id: &str,
             params: &serde_json::Value,
             signal: Option<&tokio_util::sync::CancellationToken>,
             _on_update,
             context| {
                let signal = signal.cloned();
                let context = std::sync::Arc::clone(context);
                let parsed = parse_edits(params);
                Box::pin(async move {
                    let (path, edits) = parsed?;
                    let context = as_execution_tool_context(&context)?;
                    let absolute_path = resolve_tool_path(&context.env, &path)
                        .await
                        .map_err(|error| error.to_string())?;
                    with_file_mutation_queue(&context.env, &absolute_path, || async {
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }
                        let info = context
                            .env
                            .file_info(&absolute_path, signal.clone())
                            .await
                            .map_err(|error| edit_access_error(&path, &error))?;
                        if info.kind != crate::agent::harness::types::FileKind::File
                            && info.kind != crate::agent::harness::types::FileKind::Symlink
                        {
                            return Err(format!(
                                "Could not edit file: {path}. Path is not a file."
                            ));
                        }

                        let read = context
                            .env
                            .read_text_file(&absolute_path, signal.clone())
                            .await
                            .map_err(|error| edit_access_error(&path, &error))?;
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }

                        let (bom, content) = strip_bom(&read);
                        let original_ending = detect_line_ending(content);
                        let normalized_content = normalize_to_lf(content);
                        let applied = apply_edits_to_normalized_content(
                            &normalized_content,
                            &edits,
                            &path,
                        )?;
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }

                        let final_content = format!(
                            "{bom}{}",
                            restore_line_endings(&applied.new_content, original_ending)
                        );
                        context
                            .env
                            .write_file(
                                &absolute_path,
                                &WriteContent::Text(final_content),
                                signal.clone(),
                            )
                            .await
                            .map_err(|error| edit_access_error(&path, &error))?;
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }

                        let (diff, first_changed_line) = generate_diff_string(
                            &applied.base_content,
                            &applied.new_content,
                            4,
                        );
                        let patch =
                            generate_unified_patch(&path, &applied.base_content, &applied.new_content, 4);
                        Ok(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: format!(
                                    "Successfully replaced {} block(s) in {path}.",
                                    edits.len()
                                ),
                                ..Default::default()
                            })],
                            details: serde_json::json!({
                                "diff": diff,
                                "patch": patch,
                                "firstChangedLine": first_changed_line,
                            }),
                            ..Default::default()
                        })
                    })
                    .await
                })
            },
        ),
    }
}
