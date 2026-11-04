//! The agent's background process tools
//! (ADR-background-processes-are-chat-owned): `spawn_background`,
//! `process_status`, and `kill_background_process`, all scoped to the
//! Turn's chat. A spawned process is detached from the Turn — interrupting
//! or ending the Turn never kills it; it dies by the user's hand (the
//! Processes panel), the agent's (`kill_background_process`), the chat's
//! deletion, or the app's exit.
//!
//! `spawn_background` is a mutating name (it executes a command), so it
//! meets the permission gate like `bash`; the other two only read or end
//! processes this registry already owns, and never meet it.

use std::sync::Arc;

use futures::future::BoxFuture;
use holt_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundProcesses;

/// The status tail: enough to judge a dev server ("compiled successfully",
/// "EADDRINUSE"), never a full log dump.
const STATUS_TAIL_LINES: usize = 200;
const STATUS_TAIL_BYTES: usize = 16 * 1024;

const SPAWN_DESCRIPTION: &str = "Run a long-running command in the background and return \
immediately: dev servers, file watchers, anything that must keep running past this tool call. \
The command runs detached from the conversation — interrupting or ending the Turn never stops \
it; it keeps running until you stop it with kill_background_process, the user stops it from the \
Processes panel, the chat is deleted, or the app exits. Do NOT append `&` or nohup — the \
command is already backgrounded. Do NOT use this for one-shot commands (builds, tests, \
installs): the plain bash tool returns their output directly. After starting a service, verify \
it with process_status before claiming it works — check for readiness lines and port errors. \
The user sees every background process you start.";

const STATUS_DESCRIPTION: &str = "One-shot snapshot of this chat's background processes: \
running state, exit code, and the tail of each process's output. Pass no process_id to list \
every process of this chat with a one-line status each; pass one for the full tail (last 200 \
lines). A one-shot read — do not poll in a loop: check once after spawning or when you need to \
know whether a process is still alive. Exited processes stay listed with their exit code until \
the user clears them.";

const KILL_DESCRIPTION: &str = "Stop one of this chat's background processes (graceful TERM, \
then KILL of the whole process group). Use it to clean up a server you started when it is no \
longer needed, or to restart a broken one (kill, then spawn_background again).";

fn spawn_tool(
    background: Arc<BackgroundProcesses>,
    chat_id: String,
    cwd: String,
    shell: String,
    shell_groups: Option<std::path::PathBuf>,
) -> AgentTool {
    AgentTool {
        name: "spawn_background".into(),
        label: "Spawn background".into(),
        description: SPAWN_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The long-running command, exactly as bash would run it (no trailing &)"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let background = background.clone();
                let chat_id = chat_id.clone();
                let cwd = cwd.clone();
                let shell = shell.clone();
                let shell_groups = shell_groups.clone();
                let command = params
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                Box::pin(async move {
                    let status =
                        match background.spawn(chat_id, command, &cwd, &shell, shell_groups) {
                            Ok(status) => status,
                            Err(error) => return Err(error.to_string()),
                        };
                    let text = format!(
                        "Background process {} started (pid {}). It is detached from this \
                         conversation — verify readiness with process_status before relying on \
                         it, and stop it with kill_background_process when it is no longer \
                         needed.",
                        status.process_id,
                        status
                            .pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "unknown".into()),
                    );
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text,
                            ..Default::default()
                        })],
                        details: json!({ "processId": status.process_id, "pid": status.pid }),
                        ..Default::default()
                    })
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

fn format_one(status: &holt_rpc::background::BackgroundProcessStatus) -> String {
    let state = if status.running {
        format!(
            "running (pid {})",
            status
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "?".into())
        )
    } else {
        format!(
            "exited with code {}",
            status
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        )
    };
    format!("- {} [{state}] `{}`", status.process_id, status.command)
}

fn status_tool(background: Arc<BackgroundProcesses>, chat_id: String) -> AgentTool {
    AgentTool {
        name: "process_status".into(),
        label: "Process status".into(),
        description: STATUS_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "One process's id (from spawn_background); omit to list all of this chat's processes"
                }
            },
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let background = background.clone();
                let chat_id = chat_id.clone();
                let process_id = params
                    .get("process_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                Box::pin(async move {
                    let all = background.list();
                    let mine: Vec<_> = all
                        .iter()
                        .filter(|status| status.chat_id == chat_id)
                        .collect();
                    if mine.is_empty() {
                        return Ok(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: "No background processes for this chat.".into(),
                                ..Default::default()
                            })],
                            ..Default::default()
                        });
                    }
                    let text = match process_id.as_deref() {
                        Some(id) => {
                            let Some(status) = mine.iter().find(|status| status.process_id == id)
                            else {
                                return Err(format!(
                                    "no background process {id} in this chat — the running \
                                     ones are:\n{}",
                                    mine.iter()
                                        .map(|s| format_one(s))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                ));
                            };
                            let state = if status.running {
                                "running".to_string()
                            } else {
                                format!(
                                    "exited with code {}",
                                    status
                                        .exit_code
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "unknown".into())
                                )
                            };
                            // The tail lives in the registry's log; re-read
                            // through the process entry.
                            let tail = background
                                .tail(id, STATUS_TAIL_LINES, STATUS_TAIL_BYTES)
                                .unwrap_or_default();
                            let mut text = format!("`{}` — {state}\n", status.command);
                            if tail.trim().is_empty() {
                                text.push_str("(no output captured)");
                            } else {
                                text.push_str("Last output:\n");
                                text.push_str(&tail);
                            }
                            text
                        }
                        None => mine
                            .iter()
                            .map(|status| format_one(status))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text,
                            ..Default::default()
                        })],
                        ..Default::default()
                    })
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

fn kill_tool(background: Arc<BackgroundProcesses>, chat_id: String) -> AgentTool {
    AgentTool {
        name: "kill_background_process".into(),
        label: "Kill background process".into(),
        description: KILL_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The process to stop, from spawn_background or process_status"
                }
            },
            "required": ["process_id"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let background = background.clone();
                let chat_id = chat_id.clone();
                let process_id = params
                    .get("process_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                Box::pin(async move {
                    let owned = background
                        .list()
                        .iter()
                        .any(|status| status.chat_id == chat_id && status.process_id == process_id);
                    if !owned {
                        return Err(format!("no background process {process_id} in this chat"));
                    }
                    match background.kill(&process_id) {
                        Ok(()) => Ok(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: format!(
                                    "Stopping background process {process_id} (TERM → KILL). It disappears from the running list as it exits."
                                ),
                                ..Default::default()
                            })],
                            ..Default::default()
                        }),
                        Err(error) => Err(error.to_string()),
                    }
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// The chat's background process toolset — mounted for every Turn that
/// keeps the agent shell (main chat and worker subagents; the plan-mode,
/// Provider Mode, and Explorer retains drop them with everything else that
/// executes).
pub(crate) fn create_background_tools(
    background: Arc<BackgroundProcesses>,
    chat_id: String,
    cwd: &str,
    exec_shell: Option<String>,
    shell_groups: Option<std::path::PathBuf>,
) -> Vec<AgentTool> {
    let shell = super::resolved_agent_shell(exec_shell.as_deref());
    vec![
        spawn_tool(
            background.clone(),
            chat_id.clone(),
            cwd.to_string(),
            shell,
            shell_groups,
        ),
        status_tool(background.clone(), chat_id.clone()),
        kill_tool(background, chat_id),
    ]
}
