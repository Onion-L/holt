//! Port of `pi-core/agent/src/harness/tools/read.ts`.

use std::sync::Arc;

use crate::agent::harness::types::AgentToolResult;
use crate::agent::harness::utils::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, format_size,
    truncate_head,
};
use crate::ai::types::{BlockContent, ImageContent, TextContent};

use super::image::{detect_supported_image_mime_type, encode_base64};
use super::path_utils::resolve_read_tool_path;
use super::tool_context::as_execution_tool_context;

/// Port of `ReadToolInput`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReadToolInput {
    pub path: String,
    pub offset: Option<f64>,
    pub limit: Option<f64>,
}

/// Port of `ReadToolDetails`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReadToolDetails {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub truncation: Option<crate::agent::harness::utils::truncate::TruncationResult>,
}

pub fn read_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "number", "description": "Maximum number of lines to read" }
        }
    })
}
/// Port of `ReadImageProcessorResult`.
pub enum ReadImageProcessorResult {
    Ok {
        data: String,
        mime_type: String,
        hints: Vec<String>,
    },
    Err(String),
}

/// Port of `ReadImageProcessor`.
pub type ReadImageProcessor = Arc<
    dyn Fn(&[u8], &str, bool) -> futures::future::BoxFuture<'static, ReadImageProcessorResult>
        + Send
        + Sync,
>;

/// Port of `ReadToolOptions`.
///
/// Deviation from TypeScript v0.84.4: `max_lines`/`max_bytes` override the
/// read tool's truncation limits (TypeScript hardcodes `DEFAULT_MAX_LINES`
/// and `DEFAULT_MAX_BYTES`; upstream keeps customization to extensions that
/// replace the tool — e.g. earendil-works/pi#7066). Both default to `None`,
/// which reproduces the TypeScript behavior byte-for-byte, limits and
/// truncation messages included. Focused tests in `tests/harness_tools.rs`.
#[derive(Clone, Default)]
pub struct ReadToolOptions {
    pub auto_resize_images: Option<bool>,
    pub image_processor: Option<ReadImageProcessor>,
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

/// Port of `createReadTool`.
pub fn create_read_tool(
    options: ReadToolOptions,
) -> crate::agent::harness::types::AgentHarnessTool {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let description = format!(
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        max_lines,
        max_bytes / 1024
    );
    crate::agent::harness::types::AgentHarnessTool {
        name: "read".to_string(),
        label: "read".to_string(),
        description,
        parameters: read_schema(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        replay: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&tokio_util::sync::CancellationToken>,
                  _on_update,
                  context| {
                let options = options.clone();
                let context = std::sync::Arc::clone(context);
                let path = params
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let offset = params.get("offset").and_then(|value| value.as_f64());
                let limit = params.get("limit").and_then(|value| value.as_f64());
                let signal = signal.cloned();
                Box::pin(async move {
                    let context = as_execution_tool_context(&context)?;
                    let absolute_path = resolve_read_tool_path(&context.env, &path)
                        .await
                        .map_err(|error| error.to_string())?;
                    let bytes = context
                        .env
                        .read_binary_file(&absolute_path, signal.clone())
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
                        if let Some(image_processor) = &options.image_processor {
                            let processed = image_processor(
                                &bytes,
                                mime_type,
                                options.auto_resize_images.unwrap_or(true),
                            )
                            .await;
                            match processed {
                                ReadImageProcessorResult::Err(message) => {
                                    return Ok(AgentToolResult {
                                        content: vec![BlockContent::Text(TextContent {
                                            text: format!(
                                                "Read image file [{mime_type}]\n{message}"
                                            ),
                                            ..Default::default()
                                        })],
                                        details: serde_json::Value::Null,
                                        ..Default::default()
                                    });
                                }
                                ReadImageProcessorResult::Ok {
                                    data,
                                    mime_type,
                                    hints,
                                } => {
                                    let hint_text = if hints.is_empty() {
                                        String::new()
                                    } else {
                                        format!("\n{}", hints.join("\n"))
                                    };
                                    return Ok(AgentToolResult {
                                        content: vec![
                                            BlockContent::Text(TextContent {
                                                text: format!(
                                                    "Read image file [{mime_type}]{hint_text}"
                                                ),
                                                ..Default::default()
                                            }),
                                            BlockContent::Image(ImageContent {
                                                data,
                                                mime_type,
                                                ..Default::default()
                                            }),
                                        ],
                                        details: serde_json::Value::Null,
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                        if mime_type == "image/bmp" {
                            return Ok(AgentToolResult {
                                content: vec![BlockContent::Text(TextContent {
                                    text: "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]".to_string(),
                                    ..Default::default()
                                })],
                                details: serde_json::Value::Null,
                                ..Default::default()
                            });
                        }
                        return Ok(AgentToolResult {
                            content: vec![
                                BlockContent::Text(TextContent {
                                    text: format!("Read image file [{mime_type}]"),
                                    ..Default::default()
                                }),
                                BlockContent::Image(ImageContent {
                                    data: encode_base64(&bytes),
                                    mime_type: mime_type.to_string(),
                                    ..Default::default()
                                }),
                            ],
                            details: serde_json::Value::Null,
                            ..Default::default()
                        });
                    }

                    // Text path.
                    let text_content = String::from_utf8_lossy(&bytes).into_owned();
                    let all_lines: Vec<&str> = text_content.split('\n').collect();
                    let total_file_lines = all_lines.len();
                    let start_line = offset
                        .map(|offset| (offset.max(1.0) as usize).saturating_sub(1))
                        .unwrap_or(0);
                    let start_line_display = start_line + 1;
                    if start_line >= all_lines.len() {
                        return Err(format!(
                            "Offset {} is beyond end of file ({} lines total)",
                            offset.unwrap_or_default() as usize,
                            all_lines.len()
                        ));
                    }

                    let mut user_limited_lines: Option<usize> = None;
                    let selected_content = if let Some(limit) = limit {
                        let limit = (limit.max(0.0) as usize).max(1);
                        let end_line = (start_line + limit).min(all_lines.len());
                        user_limited_lines = Some(end_line - start_line);
                        all_lines[start_line..end_line].join("\n")
                    } else {
                        all_lines[start_line..].join("\n")
                    };

                    let truncation = truncate_head(
                        &selected_content,
                        TruncationOptions {
                            max_lines: Some(max_lines),
                            max_bytes: Some(max_bytes),
                        },
                    );
                    let mut details = serde_json::Value::Null;
                    let output_text;
                    if truncation.first_line_exceeds_limit {
                        let first_line_size = format_size(all_lines[start_line].len() as u64);
                        output_text = format!(
                            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {max_bytes}]",
                            format_size(max_bytes as u64)
                        );
                        details = serde_json::json!({ "truncation": truncation });
                    } else if truncation.truncated {
                        let end_line_display = start_line_display + truncation.output_lines - 1;
                        let next_offset = end_line_display + 1;
                        let mut text = truncation.content.clone();
                        if truncation.truncated_by == Some(TruncatedBy::Lines) {
                            text.push_str(&format!(
                                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
                            ));
                        } else {
                            text.push_str(&format!(
                                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                                format_size(max_bytes as u64)
                            ));
                        }
                        output_text = text;
                        details = serde_json::json!({ "truncation": truncation });
                    } else if let Some(user_limited_lines) = user_limited_lines
                        && start_line + user_limited_lines < all_lines.len()
                    {
                        let remaining = all_lines.len() - (start_line + user_limited_lines);
                        let next_offset = start_line + user_limited_lines + 1;
                        output_text = format!(
                            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                            truncation.content
                        );
                    } else {
                        output_text = truncation.content.clone();
                    }

                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: output_text,
                            ..Default::default()
                        })],
                        details,
                        ..Default::default()
                    })
                })
            },
        ),
    }
}
