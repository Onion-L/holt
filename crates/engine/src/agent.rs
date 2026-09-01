//! The single-agent run loop over pi-core: per-chat runtime state,
//! event-to-transcript translation, and history persistence.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, TranscriptFrame,
    sanitize_tool_call, summarize_tool_output,
};
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

pub(crate) struct ChatRuntime {
    pub(crate) transcript: RwLock<Vec<SessionMessageEntry>>,
    history: RwLock<Vec<AgentMessage>>,
    pub(crate) transcript_tx: watch::Sender<serde_json::Value>,
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
}

impl ChatRuntime {
    fn new() -> Self {
        let initial = serde_json::to_value(TranscriptFrame::reset(&[])).unwrap();
        let (transcript_tx, _) = watch::channel(initial);
        Self {
            transcript: RwLock::new(Vec::new()),
            history: RwLock::new(Vec::new()),
            transcript_tx,
            cancel: Mutex::new(None),
        }
    }

    pub(crate) fn publish(&self) {
        let transcript = self.transcript.read().unwrap_or_else(|e| e.into_inner());
        if let Ok(value) = serde_json::to_value(TranscriptFrame::reset(&transcript)) {
            self.transcript_tx.send_replace(value);
        }
    }
}

pub(crate) struct AgentRuntime {
    device_id: String,
    pub(crate) chats: RwLock<Vec<Chat>>,
    pub(crate) chats_tx: watch::Sender<serde_json::Value>,
    sessions: RwLock<Vec<Session>>,
    pub(crate) sessions_tx: watch::Sender<serde_json::Value>,
    chat_runtime: Mutex<HashMap<String, Arc<ChatRuntime>>>,
}

impl AgentRuntime {
    pub(crate) fn new(device_id: String, chats: Vec<Chat>) -> Self {
        let chats_value = serde_json::to_value(&chats).unwrap_or_else(|_| serde_json::json!([]));
        let (chats_tx, _) = watch::channel(chats_value);
        let (sessions_tx, _) = watch::channel(serde_json::json!([]));
        Self {
            device_id,
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
            .or_insert_with(|| Arc::new(ChatRuntime::new()))
            .clone()
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

/// The one-line output summary persisted on the resolved tool part.
fn tool_output_summary(result: &AgentToolResult) -> Option<String> {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.as_str()),
            BlockContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    summarize_tool_output(&text)
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

fn assistant_parts(message: &AgentMessage) -> Vec<MessagePart> {
    let AgentMessage::Assistant(message) = message else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            AssistantContent::Text(text) => parts.push(MessagePart::Text {
                id: format!("t{}", parts.len()),
                text: text.text.clone(),
            }),
            AssistantContent::Thinking(thinking) if !thinking.thinking.is_empty() => {
                parts.push(MessagePart::Reasoning {
                    id: format!("r{}", parts.len()),
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
            id: format!("e{}", parts.len()),
            message: error.clone(),
        });
    }
    parts
}

fn update_assistant_entry(
    chat: &ChatRuntime,
    entry_id: &str,
    message: &AgentMessage,
    status: MessageStatus,
    device_id: &str,
) {
    let parts = assistant_parts(message);
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let entry = SessionMessageEntry {
        id: entry_id.to_string(),
        role: MessageRole::Assistant,
        parts,
        created_at: Utc::now().timestamp_millis(),
        device_id: device_id.to_string(),
        status: Some(status),
        continuation_of: None,
    };
    if let Some(existing) = transcript.iter_mut().find(|entry| entry.id == entry_id) {
        *existing = entry;
    } else {
        transcript.push(entry);
    }
    drop(transcript);
    chat.publish();
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
    // One transcript entry per assistant message — every tool round-trip
    // adds another, so the live entry id rotates on each MessageStart.
    // (Tool results update existing parts by tool-call id, not by entry.)
    let live_entry: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink_live = live_entry.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        let chat = sink_chat.clone();
        let live_entry = sink_live.clone();
        let device_id = sink_device_id.clone();
        let run_entry = sink_run_entry.clone();
        Box::pin(async move {
            match event {
                AgentEvent::MessageStart { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let entry_id = uuid::Uuid::new_v4().to_string();
                    *live_entry.lock().unwrap_or_else(|e| e.into_inner()) = Some(entry_id.clone());
                    update_assistant_entry(
                        &chat,
                        &entry_id,
                        &message,
                        MessageStatus::Streaming,
                        &device_id,
                    );
                }
                AgentEvent::MessageUpdate { message, .. }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let entry_id = live_entry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                        .unwrap_or(run_entry);
                    update_assistant_entry(
                        &chat,
                        &entry_id,
                        &message,
                        MessageStatus::Streaming,
                        &device_id,
                    );
                }
                AgentEvent::MessageEnd { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let entry_id = live_entry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                        .unwrap_or(run_entry);
                    update_assistant_entry(
                        &chat,
                        &entry_id,
                        &message,
                        MessageStatus::Complete,
                        &device_id,
                    );
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    is_error,
                    ..
                } => {
                    resolve_tool_part(&chat, &tool_call_id, is_error, tool_output_summary(&result));
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
            assistant_parts(&message),
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
            assistant_parts(&message),
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
}
