//! The single-agent run loop over pi-core: per-chat runtime state,
//! event-to-transcript translation, and history persistence.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, sanitize_tool_call};
use holt_proto::{Chat, ReasoningLevel, Session, SessionStatus, ToolCall as TranscriptToolCall};
use pi_core::{
    agent::{
        agent_loop::{AgentEventSink, pass_through_llm_messages, run_agent_loop},
        types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentToolResult},
    },
    ai::{
        compat,
        types::{
            AssistantContent, BlockContent, Context as PiContext, Model as PiModel, RoleUser,
            SimpleStreamOptions, ThinkingLevel as ProviderThinkingLevel, UserContent, UserMessage,
        },
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::store::{delete_transcript, load_transcript, persist_transcript};

pub(crate) struct ChatRuntime {
    pub(crate) transcript: RwLock<Vec<SessionMessageEntry>>,
    history: RwLock<Vec<AgentMessage>>,
    pub(crate) transcript_tx: watch::Sender<Arc<Vec<SessionMessageEntry>>>,
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
    /// Where this chat's transcript persists; empty for the ephemeral
    /// runtimes tests build directly.
    data_dir: PathBuf,
    chat_id: String,
}

/// Streaming publishes sample to this cadence (the doc-watch commit tick the
/// UI was tuned around): the watch itself coalesces via `send_replace`, so a
/// publish per delta token would only burn a full transcript snapshot per
/// token on the engine thread and starve the SSE pump.
const STREAM_PUBLISH_INTERVAL: Duration = Duration::from_millis(120);

impl ChatRuntime {
    #[cfg(test)]
    fn new() -> Self {
        let (transcript_tx, _) = watch::channel(Arc::new(Vec::new()));
        Self {
            transcript: RwLock::new(Vec::new()),
            history: RwLock::new(Vec::new()),
            transcript_tx,
            cancel: Mutex::new(None),
            data_dir: PathBuf::new(),
            chat_id: String::new(),
        }
    }

    /// A chat whose transcript persists under `data_dir`, seeded from its
    /// last on-disk snapshot so reopening after a restart restores the
    /// conversation. A corrupt file starts empty rather than failing the
    /// open; the next publish overwrites it.
    fn load(data_dir: &Path, chat_id: &str) -> Self {
        let transcript = load_transcript(data_dir, chat_id).unwrap_or_default();
        // The channel's initial value is the first frame subscribers see, so
        // seed it with the restored transcript: opening the watch replays it
        // as a whole-transcript `reset` without needing a publish.
        let (transcript_tx, _) = watch::channel(Arc::new(transcript.clone()));
        Self {
            transcript: RwLock::new(transcript),
            history: RwLock::new(Vec::new()),
            transcript_tx,
            cancel: Mutex::new(None),
            data_dir: data_dir.to_path_buf(),
            chat_id: chat_id.to_string(),
        }
    }

    pub(crate) fn publish(&self) {
        let transcript = self.transcript.read().unwrap_or_else(|e| e.into_inner());
        self.transcript_tx
            .send_replace(Arc::new(transcript.clone()));
        // Best-effort snapshot: the in-memory watch stays authoritative, and
        // a failed write surfaces again on the next publish instead of
        // failing the run that triggered it.
        if !self.chat_id.is_empty() {
            let _ = persist_transcript(&self.data_dir, &self.chat_id, &transcript);
        }
    }
}

pub(crate) struct AgentRuntime {
    device_id: String,
    data_dir: PathBuf,
    pub(crate) chats: RwLock<Vec<Chat>>,
    pub(crate) chats_tx: watch::Sender<serde_json::Value>,
    sessions: RwLock<Vec<Session>>,
    pub(crate) sessions_tx: watch::Sender<serde_json::Value>,
    chat_runtime: Mutex<HashMap<String, Arc<ChatRuntime>>>,
}

impl AgentRuntime {
    pub(crate) fn new(device_id: String, data_dir: PathBuf, chats: Vec<Chat>) -> Self {
        let chats_value = serde_json::to_value(&chats).unwrap_or_else(|_| serde_json::json!([]));
        let (chats_tx, _) = watch::channel(chats_value);
        let (sessions_tx, _) = watch::channel(serde_json::json!([]));
        Self {
            device_id,
            data_dir,
            chats: RwLock::new(chats),
            chats_tx,
            sessions: RwLock::new(Vec::new()),
            sessions_tx,
            chat_runtime: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn chat(&self, chat_id: &str) -> Arc<ChatRuntime> {
        let mut chats = self.chat_runtime.lock().unwrap_or_else(|e| e.into_inner());
        chats
            .entry(chat_id.to_string())
            .or_insert_with(|| Arc::new(ChatRuntime::load(&self.data_dir, chat_id)))
            .clone()
    }

    /// Drop a chat's runtime slot and its persisted transcript. An in-flight
    /// run keeps its `Arc` and runs to completion, but nothing ever reads the
    /// transcript again: the chat row is gone from the watches.
    pub(crate) fn remove_chat(&self, chat_id: &str) {
        self.chat_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(chat_id);
        delete_transcript(&self.data_dir, chat_id);
    }

    pub(crate) fn publish_chats(&self) {
        let chats = self.chats.read().unwrap_or_else(|e| e.into_inner());
        if let Ok(value) = serde_json::to_value(&*chats) {
            self.chats_tx.send_replace(value);
        }
    }

    pub(crate) fn set_session(&self, chat_id: &str, status: SessionStatus) {
        let now = Utc::now();
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.chat_id == chat_id)
        {
            session.status = status;
            session.updated_at = now;
            if status == SessionStatus::Working && session.started_at.is_none() {
                session.started_at = Some(now);
            }
        } else {
            sessions.push(Session {
                chat_id: chat_id.to_string(),
                device_id: self.device_id.clone(),
                status,
                started_at: (status == SessionStatus::Working).then_some(now),
                updated_at: now,
            });
        }
        if let Ok(value) = serde_json::to_value(&*sessions) {
            self.sessions_tx.send_replace(value);
        }
    }
}

fn provider_reasoning(level: Option<ReasoningLevel>) -> Option<ProviderThinkingLevel> {
    level.map(|level| match level {
        ReasoningLevel::Minimal => ProviderThinkingLevel::Minimal,
        ReasoningLevel::Low => ProviderThinkingLevel::Low,
        ReasoningLevel::Medium => ProviderThinkingLevel::Medium,
        ReasoningLevel::High => ProviderThinkingLevel::High,
        ReasoningLevel::XHigh => ProviderThinkingLevel::Xhigh,
        ReasoningLevel::Max | ReasoningLevel::Ultra | ReasoningLevel::Ultracode => {
            ProviderThinkingLevel::Max
        }
        ReasoningLevel::Ultrathink => ProviderThinkingLevel::High,
    })
}

fn user_agent_message(text: String, timestamp: i64) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text),
        timestamp,
    })
}

/// Decode a pi-core tool call into the transcript's decoded shape. Heavy
/// inputs (write content, edit strings) decode here and are stripped by the
/// render-only policy below; unknown tools keep their raw input so the chip
/// can still name them.
fn decode_tool_call(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> TranscriptToolCall {
    let arg = |key: &str| {
        arguments
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match name {
        "bash" => TranscriptToolCall::Exec {
            command: arg("command").unwrap_or_default(),
        },
        "read" => TranscriptToolCall::ReadFile {
            path: arg("path").unwrap_or_default(),
        },
        "write" => TranscriptToolCall::WriteFile {
            path: arg("path").unwrap_or_default(),
            content: None,
        },
        "edit" => TranscriptToolCall::EditFile {
            path: arg("path").unwrap_or_default(),
            old_string: None,
            new_string: None,
        },
        other => TranscriptToolCall::Unknown {
            name: other.to_owned(),
            input: Some(serde_json::Value::Object(arguments.clone())),
        },
    }
}

fn transcript_tool_call(tool_call: &pi_core::ai::types::ToolCall) -> TranscriptToolCall {
    sanitize_tool_call(&decode_tool_call(&tool_call.name, &tool_call.arguments))
}

/// The full tool output persisted on the resolved tool part — a Read's file
/// content, the whole command transcript. pi-core bounds its builtins (read
/// truncates by lines/bytes), so results ride verbatim; the defensive ceiling
/// only keeps an unbounded MCP payload from flooding the doc.
fn tool_output_full(result: &AgentToolResult) -> Option<String> {
    const MAX_CHARS: usize = 1024 * 1024;
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.as_str()),
            BlockContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.chars().count() <= MAX_CHARS {
        return (!text.trim().is_empty()).then_some(text);
    }
    let mut out: String = text.chars().take(MAX_CHARS).collect();
    out.push_str("\n…");
    Some(out)
}

/// Stamp a tool result onto the matching Tool part, wherever its entry sits.
fn resolve_tool_part(
    chat: &ChatRuntime,
    tool_call_id: &str,
    is_error: bool,
    output: Option<String>,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for entry in transcript.iter_mut() {
        let hit = entry
            .parts
            .iter_mut()
            .find(|part| matches!(part, MessagePart::Tool { id, .. } if id == tool_call_id));
        if let Some(MessagePart::Tool {
            resolved,
            is_error: part_error,
            output: part_output,
            ..
        }) = hit
        {
            *resolved = true;
            *part_error = is_error;
            *part_output = output.clone();
            changed = true;
        }
        if changed {
            break;
        }
    }
    drop(transcript);
    if changed {
        chat.publish();
    }
}

/// A run that ends early (abort, loop error) leaves tool parts without their
/// results; settle them so no chip stays "in call" forever.
fn settle_unresolved_tools(chat: &ChatRuntime) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for entry in transcript.iter_mut() {
        for part in entry.parts.iter_mut() {
            if let MessagePart::Tool { resolved, .. } = part
                && !*resolved
            {
                *resolved = true;
                changed = true;
            }
        }
    }
    drop(transcript);
    if changed {
        chat.publish();
    }
}

/// char length of a part for the run-cadence debug trace.
fn part_char_len(part: &MessagePart) -> usize {
    match part {
        MessagePart::Text { text, .. } | MessagePart::Reasoning { text, .. } => text.len(),
        MessagePart::Error { message, .. } => message.len(),
        _ => 0,
    }
}

/// The doc parts of one assistant message. `id_base` offsets the generated
/// text/thinking/error part ids: a run folds every message into ONE entry, so
/// per-message ids (`t0`, `r1`, …) must not collide across its messages — the
/// UI keys rows by `entry_id#part_id`.
fn assistant_parts(message: &AgentMessage, id_base: usize) -> Vec<MessagePart> {
    let AgentMessage::Assistant(message) = message else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            AssistantContent::Text(text) => parts.push(MessagePart::Text {
                id: format!("t{}", id_base + parts.len()),
                text: text.text.clone(),
            }),
            AssistantContent::Thinking(thinking) if !thinking.thinking.is_empty() => {
                parts.push(MessagePart::Reasoning {
                    id: format!("r{}", id_base + parts.len()),
                    text: thinking.thinking.clone(),
                });
            }
            AssistantContent::ToolCall(tool_call) => parts.push(MessagePart::Tool {
                id: tool_call.id.clone(),
                call: transcript_tool_call(tool_call),
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }),
            AssistantContent::Thinking(_) => {}
        }
    }
    if let Some(error) = message.error_message.as_ref() {
        parts.push(MessagePart::Error {
            id: format!("e{}", id_base + parts.len()),
            message: error.clone(),
        });
    }
    parts
}

/// Insert or refresh the run's live entry. `created_at` is stamped once at
/// first appearance — the delta protocol keys appends off an unchanged entry,
/// and the hover timestamp should say when the reply started anyway.
fn update_assistant_entry(
    chat: &ChatRuntime,
    entry_id: &str,
    parts: Vec<MessagePart>,
    status: MessageStatus,
    device_id: &str,
    publish: bool,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = transcript.iter_mut().find(|entry| entry.id == entry_id) {
        existing.parts = parts;
        existing.status = Some(status);
    } else {
        transcript.push(SessionMessageEntry {
            id: entry_id.to_string(),
            role: MessageRole::Assistant,
            parts,
            created_at: Utc::now().timestamp_millis(),
            device_id: device_id.to_string(),
            status: Some(status),
            continuation_of: None,
        });
    }
    drop(transcript);
    if publish {
        chat.publish();
    }
}

pub(crate) struct AgentRun {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) chat_id: String,
    pub(crate) chat: Arc<ChatRuntime>,
    pub(crate) prompt: String,
    pub(crate) cwd: String,
    pub(crate) reasoning: Option<ReasoningLevel>,
    pub(crate) model: PiModel,
    pub(crate) api_key: String,
    pub(crate) timestamp: i64,
    pub(crate) cancel: CancellationToken,
}

pub(crate) async fn run_agent_command(run: AgentRun) {
    let AgentRun {
        runtime,
        chat_id,
        chat,
        prompt,
        cwd,
        reasoning,
        model,
        api_key,
        timestamp,
        cancel,
    } = run;
    let entry_id = uuid::Uuid::new_v4().to_string();
    let sink_chat = chat.clone();
    let sink_device_id = runtime.device_id.clone();
    let sink_run_entry = entry_id.clone();
    // ONE transcript entry per run: every assistant message of the loop
    // appends its parts to the same entry (base holds the parts of the
    // messages that already ended), so a reply with N tool round-trips
    // renders as one message — one turn gap, one hover timestamp/copy strip
    // at its end. Per-message entries stamped a strip mid-reply after every
    // round-trip, which read as several half-finished replies (user report),
    // and tool results update parts by tool-call id wherever they sit.
    let base_parts: Arc<Mutex<Vec<MessagePart>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_base = base_parts.clone();
    let sink_last_publish = Arc::new(Mutex::new(None::<Instant>));
    let sink_run_start = Instant::now();
    let emit: AgentEventSink = Arc::new(move |event| {
        let chat = sink_chat.clone();
        let base_parts = sink_base.clone();
        let device_id = sink_device_id.clone();
        let run_entry = sink_run_entry.clone();
        let last_publish = sink_last_publish.clone();
        Box::pin(async move {
            // Debug trace of the event cadence: answers "did the reply
            // stream?" without a debugger — deltas arriving bunched here are
            // an upstream (provider/pi-core) shape, not a UI problem.
            let trace = |kind: &str, chars: usize, published: bool| {
                tracing::debug!(
                    target: "holt::agent",
                    at = ?sink_run_start.elapsed(),
                    kind,
                    chars,
                    published,
                    "run event"
                );
            };
            match event {
                AgentEvent::MessageStart { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len()));
                    drop(base);
                    trace("start", parts.iter().map(part_char_len).sum(), true);
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        true,
                    );
                }
                AgentEvent::MessageUpdate { message, .. }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let due = {
                        let mut last = last_publish.lock().unwrap_or_else(|e| e.into_inner());
                        if last.is_none_or(|at| at.elapsed() >= STREAM_PUBLISH_INTERVAL) {
                            *last = Some(Instant::now());
                            true
                        } else {
                            false
                        }
                    };
                    let base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len()));
                    drop(base);
                    trace("delta", parts.iter().map(part_char_len).sum(), due);
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        due,
                    );
                }
                AgentEvent::MessageEnd { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let mut base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len()));
                    *base = parts.clone();
                    drop(base);
                    // The next message's first delta must publish immediately.
                    *last_publish.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    // Status stays Streaming: the loop's settle pass stamps
                    // the terminal state once the WHOLE run returns, so the
                    // entry never poses as complete between tool rounds.
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        true,
                    );
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    is_error,
                    ..
                } => {
                    resolve_tool_part(&chat, &tool_call_id, is_error, tool_output_full(&result));
                }
                _ => {}
            }
        })
    });

    let history = chat
        .history
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let prompt_message = user_agent_message(prompt, timestamp);
    let mut stream_options = SimpleStreamOptions {
        reasoning: provider_reasoning(reasoning),
        ..Default::default()
    };
    stream_options.base.base.api_key = Some(api_key);
    let config = AgentLoopConfig {
        stream_options,
        model,
        convert_to_llm: Arc::new(|messages| {
            Box::pin(async move { pass_through_llm_messages(messages) })
        }),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        tool_execution: None,
        before_tool_call: None,
        after_tool_call: None,
    };
    let stream_fn = Arc::new(
        |model: &PiModel, context: &PiContext, options: Option<&SimpleStreamOptions>| {
            Ok(compat::stream_simple(model, context, options))
        },
    );
    let result = run_agent_loop(
        vec![prompt_message],
        AgentContext {
            system_prompt: format!(
                "You are a coding assistant working in {cwd}. \
                 Use the read, write, edit and bash tools to inspect and \
                 change files whenever the task needs it."
            ),
            messages: history.clone(),
            tools: Some(crate::tools::execution_tools(&cwd)),
        },
        config,
        emit,
        Some(cancel.clone()),
        Some(stream_fn),
    )
    .await;

    let errored = match result {
        Ok(messages) => {
            let errored = messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    AgentMessage::Assistant(message) => Some(message.error_message.is_some()),
                    _ => None,
                })
                .unwrap_or(false);
            let mut stored = chat.history.write().unwrap_or_else(|e| e.into_inner());
            *stored = history;
            stored.extend(messages);
            drop(stored);
            // An interrupted loop never sends the closing MessageEnd, so its
            // last entry would stream forever — settle it here.
            let end_status = if cancel.is_cancelled() || errored {
                MessageStatus::Aborted
            } else {
                MessageStatus::Complete
            };
            let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
            let mut settled = false;
            for entry in transcript.iter_mut() {
                if entry.status == Some(MessageStatus::Streaming) {
                    entry.status = Some(end_status);
                    settled = true;
                }
            }
            drop(transcript);
            if settled {
                chat.publish();
            }
            errored
        }
        Err(error) => {
            let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = transcript.iter_mut().find(|e| e.id == entry_id) {
                // The loop died mid-reply: surface the error on the live entry
                // and settle it, rather than pushing a second entry that
                // reuses its id.
                existing.parts.push(MessagePart::Error {
                    id: format!("e{}", existing.parts.len()),
                    message: error,
                });
                existing.status = Some(MessageStatus::Aborted);
            } else {
                transcript.push(SessionMessageEntry {
                    id: entry_id,
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Error {
                        id: "e0".into(),
                        message: error,
                    }],
                    created_at: Utc::now().timestamp_millis(),
                    device_id: runtime.device_id.clone(),
                    status: Some(MessageStatus::Complete),
                    continuation_of: None,
                });
            }
            drop(transcript);
            chat.publish();
            true
        }
    };
    settle_unresolved_tools(&chat);
    *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    runtime.set_session(
        &chat_id,
        if errored {
            SessionStatus::Errored
        } else {
            SessionStatus::Idle
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{AssistantMessage, TextContent, ThinkingContent, ToolCall};

    fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            content_type: Default::default(),
            id: "call-1".into(),
            name: name.into(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            namespace: None,
        }
    }

    #[test]
    fn assistant_message_maps_text_and_reasoning_to_doc_parts() {
        let message = AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "plan".into(),
                    ..Default::default()
                }),
                AssistantContent::Text(TextContent {
                    text: "answer".into(),
                    ..Default::default()
                }),
            ],
            ..Default::default()
        }));
        assert_eq!(
            assistant_parts(&message, 0),
            vec![
                MessagePart::Reasoning {
                    id: "r0".into(),
                    text: "plan".into(),
                },
                MessagePart::Text {
                    id: "t1".into(),
                    text: "answer".into(),
                },
            ]
        );
        // A second message of the same run folds into the same entry: its
        // generated part ids continue after the base so row keys never
        // collide.
        assert_eq!(
            assistant_parts(&message, 2),
            vec![
                MessagePart::Reasoning {
                    id: "r2".into(),
                    text: "plan".into(),
                },
                MessagePart::Text {
                    id: "t3".into(),
                    text: "answer".into(),
                },
            ]
        );
    }

    #[test]
    fn assistant_message_maps_tool_calls_to_tool_parts() {
        let message = AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![AssistantContent::ToolCall(tool_call(
                "bash",
                serde_json::json!({ "command": "ls -la" }),
            ))],
            ..Default::default()
        }));
        assert_eq!(
            assistant_parts(&message, 0),
            vec![MessagePart::Tool {
                id: "call-1".into(),
                call: TranscriptToolCall::Exec {
                    command: "ls -la".into(),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }]
        );
    }

    #[test]
    fn tool_calls_decode_to_known_shapes_and_strip_heavy_inputs() {
        assert_eq!(
            transcript_tool_call(&tool_call("read", serde_json::json!({ "path": "a.rs" }))),
            TranscriptToolCall::ReadFile {
                path: "a.rs".into()
            }
        );
        // Write content is stripped before it can reach the doc.
        assert_eq!(
            transcript_tool_call(&tool_call(
                "write",
                serde_json::json!({ "path": "a.rs", "content": "lots of text" })
            )),
            TranscriptToolCall::WriteFile {
                path: "a.rs".into(),
                content: None,
            }
        );
        assert_eq!(
            transcript_tool_call(&tool_call(
                "edit",
                serde_json::json!({ "path": "a.rs", "edits": [{ "oldText": "x", "newText": "y" }] })
            )),
            TranscriptToolCall::EditFile {
                path: "a.rs".into(),
                old_string: None,
                new_string: None,
            }
        );
        // Unknown tools degrade to a named chip, input intact (the policy
        // strips non-spawn inputs).
        let decoded = transcript_tool_call(&tool_call(
            "web_search",
            serde_json::json!({ "query": "holt" }),
        ));
        assert!(matches!(
            decoded,
            TranscriptToolCall::Unknown { ref name, input: None } if name == "web_search"
        ));
    }

    #[test]
    fn transcript_survives_runtime_restart() {
        let dir = std::env::temp_dir().join(format!("holt-restart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = AgentRuntime::new("device".into(), dir.clone(), Vec::new());
        let chat = runtime.chat("chat-1");
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into(),
            }],
            created_at: 1,
            device_id: "device".into(),
            status: None,
            continuation_of: None,
        });
        chat.publish();

        // A fresh runtime over the same data dir replays the persisted
        // transcript both in memory and as the watch's opening `reset` frame.
        let restarted = AgentRuntime::new("device".into(), dir.clone(), Vec::new());
        let restored = restarted.chat("chat-1");
        assert_eq!(restored.transcript.read().unwrap().len(), 1);
        assert_eq!(restored.transcript_tx.borrow().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_tool_part_stamps_the_matching_chip() {
        let chat = ChatRuntime::new();
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: "entry-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: "call-9".into(),
                call: TranscriptToolCall::Exec {
                    command: "sleep 1".into(),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        });
        resolve_tool_part(&chat, "call-9", true, Some("boom".into()));
        let transcript = chat.transcript.read().unwrap();
        let Some(MessagePart::Tool {
            resolved,
            is_error,
            output,
            ..
        }) = transcript[0].parts.first()
        else {
            panic!("expected a tool part");
        };
        assert!(*resolved);
        assert!(*is_error);
        assert_eq!(output.as_deref(), Some("boom"));
    }

    #[test]
    fn run_messages_fold_into_one_entry_with_stable_created_at() {
        let chat = ChatRuntime::new();
        let text = |text: &str| {
            AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: text.into(),
                    ..Default::default()
                })],
                ..Default::default()
            }))
        };
        // First message creates the entry, later ones replace its parts in
        // place — same id, same created_at (the delta protocol keys appends
        // off an unchanged entry and the strip stamps once).
        let mut first_parts = assistant_parts(&text("hello"), 0);
        update_assistant_entry(
            &chat,
            "run-1",
            first_parts.clone(),
            MessageStatus::Streaming,
            "device",
            true,
        );
        let created_at = chat.transcript.read().unwrap()[0].created_at;
        let second = assistant_parts(&text(" world"), first_parts.len());
        first_parts.extend(second);
        update_assistant_entry(
            &chat,
            "run-1",
            first_parts,
            MessageStatus::Streaming,
            "device",
            false,
        );
        let transcript = chat.transcript.read().unwrap();
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].created_at, created_at);
        assert_eq!(
            transcript[0].parts,
            vec![
                MessagePart::Text {
                    id: "t0".into(),
                    text: "hello".into(),
                },
                MessagePart::Text {
                    id: "t1".into(),
                    text: " world".into(),
                },
            ]
        );
    }
}
