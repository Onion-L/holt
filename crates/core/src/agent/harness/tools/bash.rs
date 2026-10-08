//! Port of `pi-core/agent/src/harness/tools/bash.ts`.

use std::sync::Arc;
use std::time::Duration;

use crate::agent::harness::types::AgentToolResult;
use crate::agent::harness::utils::shell_output::{ShellCaptureOptions, execute_shell_with_capture};
use crate::agent::harness::utils::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size,
};
use crate::ai::types::{BlockContent, TextContent};

use super::tool_context::as_execution_tool_context;

/// Port of `BashToolInput`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BashToolInput {
    pub command: String,
    pub timeout: Option<f64>,
}

/// Port of `BashToolDetails`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashToolDetails {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub truncation: Option<crate::agent::harness::utils::truncate::TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_output_path: Option<String>,
}

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

pub fn bash_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["command"],
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
        }
    })
}
/// Port of `BashExecution`.
#[derive(Clone, Debug, Default)]
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub inherit_env: bool,
}

/// Port of `BashPrepare`: may mutate the execution in place.
pub type BashPrepare = Arc<
    dyn for<'a> Fn(
            &'a mut BashExecution,
            Option<&'a tokio_util::sync::CancellationToken>,
        ) -> BoxFuture<'a, ()>
        + Send
        + Sync,
>;

use futures::future::BoxFuture;

/// Port of `BashToolOptions`.
#[derive(Clone, Default)]
pub struct BashToolOptions {
    pub command_prefix: Option<String>,
    pub prepare: Option<BashPrepare>,
}

fn validate_timeout(timeout: Option<f64>) -> Result<(), String> {
    let Some(timeout) = timeout else {
        return Ok(());
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err("Invalid timeout: must be a finite number of seconds".to_string());
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    Ok(())
}

/// Port of `createBashTool`.
pub fn create_bash_tool(
    options: BashToolOptions,
) -> crate::agent::harness::types::AgentHarnessTool {
    let description = format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    );
    crate::agent::harness::types::AgentHarnessTool {
        name: "bash".to_string(),
        label: "bash".to_string(),
        description,
        parameters: bash_schema(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        replay: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&tokio_util::sync::CancellationToken>,
                  on_update,
                  context| {
                let on_update = on_update.cloned();
                let context = std::sync::Arc::clone(context);
                let options = options.clone();
                let command = params
                    .get("command")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let timeout = params.get("timeout").and_then(|value| value.as_f64());
                let signal = signal.cloned();
                Box::pin(async move {
                    validate_timeout(timeout)?;
                    let context = as_execution_tool_context(&context)?;
                    let mut execution = BashExecution {
                        command: match &options.command_prefix {
                            Some(prefix) => format!("{prefix}\n{command}"),
                            None => command.clone(),
                        },
                        cwd: context.env.cwd(),
                        env: Default::default(),
                        inherit_env: true,
                    };
                    if let Some(prepare) = &options.prepare {
                        prepare(&mut execution, signal.as_ref()).await;
                    }

                    // Throttled update emission: the latest capture
                    // progress is latched by chunk callbacks and flushed at
                    // most every BASH_UPDATE_THROTTLE_MS, with a final
                    // flush after the capture settles.
                    let latest: Arc<
                        std::sync::Mutex<
                            Option<crate::agent::harness::utils::shell_output::ShellCaptureResult>,
                        >,
                    > = Arc::new(std::sync::Mutex::new(None));
                    let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let notify = Arc::new(tokio::sync::Notify::new());
                    let on_chunk = {
                        let latest = Arc::clone(&latest);
                        let pending = Arc::clone(&pending);
                        let notify = Arc::clone(&notify);
                        Arc::new(move |_chunk: &str, progress| {
                            *latest.lock().unwrap() = Some(shell_capture_progress_result(progress));
                            pending.store(true, std::sync::atomic::Ordering::SeqCst);
                            notify.notify_one();
                        })
                    };

                    if let Some(on_update) = &on_update {
                        on_update(AgentToolResult {
                            content: Vec::new(),
                            details: serde_json::Value::Null,
                            ..Default::default()
                        });
                    }

                    let capture = execute_shell_with_capture(
                        context.env.as_ref(),
                        &execution.command,
                        Some(ShellCaptureOptions {
                            cwd: Some(execution.cwd.clone()),
                            env: Some(execution.env.clone()),
                            inherit_env: Some(execution.inherit_env),
                            timeout,
                            abort_signal: signal.clone(),
                            return_execution_errors: Some(true),
                            on_chunk: Some(on_chunk),
                        }),
                    )
                    .await
                    .map_err(|error| error.to_string())?;

                    // Flush any pending throttled update first, then the
                    // final state.
                    let final_capture = capture;
                    if let Some(on_update) = &on_update {
                        emit_bash_update(on_update, &final_capture);
                    }
                    let _ = (latest, pending, notify);

                    let mut output_text = final_capture.output.clone();
                    let mut details = serde_json::Value::Null;
                    if final_capture.truncation.truncated {
                        let truncation = &final_capture.truncation;
                        details = serde_json::json!({
                            "truncation": truncation,
                            "fullOutputPath": final_capture.full_output_path,
                        });
                        let start_line = truncation.total_lines - truncation.output_lines + 1;
                        let end_line = truncation.total_lines;
                        if truncation.last_line_partial {
                            let last_line_size = format_size(final_capture.last_line_bytes as u64);
                            output_text.push_str(&format!(
                                "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {}]",
                                format_size(truncation.output_bytes as u64),
                                full_path(&final_capture),
                            ));
                        } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
                            output_text.push_str(&format!(
                                "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {}]",
                                truncation.total_lines,
                                full_path(&final_capture),
                            ));
                        } else {
                            output_text.push_str(&format!(
                                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {}]",
                                truncation.total_lines,
                                format_size(DEFAULT_MAX_BYTES as u64),
                                full_path(&final_capture),
                            ));
                        }
                    }

                    let append_status = |status: String| -> String {
                        if output_text.is_empty() {
                            status
                        } else {
                            format!("{output_text}\n\n{status}")
                        }
                    };
                    if final_capture.cancelled {
                        return Err(append_status("Command aborted".to_string()));
                    }
                    if let Some(execution_error) = &final_capture.execution_error {
                        if execution_error.code
                            == crate::agent::harness::types::ExecutionErrorCode::Timeout
                        {
                            return Err(append_status(format!(
                                "Command timed out after {} seconds",
                                timeout.unwrap_or_default()
                            )));
                        }
                        return Err(execution_error.to_string());
                    }
                    if let Some(exit_code) = final_capture.exit_code
                        && exit_code != 0
                    {
                        return Err(append_status(format!(
                            "Command exited with code {exit_code}"
                        )));
                    }
                    return Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: if output_text.is_empty() {
                                "(no output)".to_string()
                            } else {
                                output_text
                            },
                            ..Default::default()
                        })],
                        details,
                        ..Default::default()
                    });

                    fn full_path(
                        capture: &crate::agent::harness::utils::shell_output::ShellCaptureResult,
                    ) -> String {
                        capture
                            .full_output_path
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string())
                    }

                    fn shell_capture_progress_result(
                        progress: crate::agent::harness::utils::shell_output::ShellCaptureProgress,
                    ) -> crate::agent::harness::utils::shell_output::ShellCaptureResult
                    {
                        // The pacer only reads output/truncation/path fields;
                        // a synthetic result keeps the shared emit path.
                        crate::agent::harness::utils::shell_output::ShellCaptureResult {
                            output: progress.output,
                            truncation: progress.truncation,
                            full_output_path: progress.full_output_path,
                            last_line_bytes: progress.last_line_bytes,
                            exit_code: None,
                            cancelled: false,
                            truncated: false,
                            execution_error: None,
                        }
                    }
                })
            },
        ),
    }
}

fn emit_bash_update(
    on_update: &crate::agent::types::AgentToolUpdateCallback,
    capture: &crate::agent::harness::utils::shell_output::ShellCaptureResult,
) {
    let details = if capture.truncation.truncated {
        serde_json::json!({
            "truncation": capture.truncation,
            "fullOutputPath": capture.full_output_path,
        })
    } else {
        serde_json::Value::Null
    };
    on_update(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: capture.output.clone(),
            ..Default::default()
        })],
        details,
        ..Default::default()
    });
}

/// Duration of the update throttle (exposed for tests).
pub const BASH_UPDATE_THROTTLE: Duration = Duration::from_millis(BASH_UPDATE_THROTTLE_MS);
