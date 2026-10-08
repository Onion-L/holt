//! Port of `pi-core/agent/src/harness/tools/write.ts`.

use std::sync::Arc;

use crate::agent::harness::types::{AgentToolResult, WriteContent};
use crate::ai::types::{BlockContent, TextContent};

use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_tool_path;
use super::tool_context::as_execution_tool_context;

/// Port of `WriteToolInput`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WriteToolInput {
    pub path: String,
    pub content: String,
}

pub fn write_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["path", "content"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        }
    })
}
/// Port of `createWriteTool`.
pub fn create_write_tool() -> crate::agent::harness::types::AgentHarnessTool {
    crate::agent::harness::types::AgentHarnessTool {
        name: "write".to_string(),
        label: "write".to_string(),
        description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string(),
        parameters: write_schema(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        replay: None,
        execute: Arc::new(
            |_tool_call_id: &str,
             params: &serde_json::Value,
             signal: Option<&tokio_util::sync::CancellationToken>,
             _on_update,
             context| {
                let path = params
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let content = params
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let signal = signal.cloned();
                let context = std::sync::Arc::clone(context);
                Box::pin(async move {
                    let context = as_execution_tool_context(&context)?;
                    let absolute_path = resolve_tool_path(&context.env, &path)
                        .await
                        .map_err(|error| error.to_string())?;
                    with_file_mutation_queue(&context.env, &absolute_path, || async {
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }
                        context
                            .env
                            .write_file(
                                &absolute_path,
                                &WriteContent::Text(content.clone()),
                                signal.clone(),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                            return Err("Operation aborted".to_string());
                        }
                        Ok(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: format!(
                                    "Successfully wrote {} bytes to {path}",
                                    content.len()
                                ),
                                ..Default::default()
                            })],
                            details: serde_json::Value::Null,
                            ..Default::default()
                        })
                    })
                    .await
                })
            },
        ),
    }
}
