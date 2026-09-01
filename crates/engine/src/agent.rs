//! The single-agent run loop over pi-core: per-chat runtime state,
//! event-to-transcript translation, and history persistence.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, TranscriptFrame};
use holt_proto::{Chat, ReasoningLevel, Session, SessionStatus};
use pi_core::{
    agent::{
        agent_loop::{AgentEventSink, pass_through_llm_messages, run_agent_loop},
        types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage},
    },
    ai::{
        compat,
        types::{
            AssistantContent, Context as PiContext, Model as PiModel, RoleUser,
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
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => {}
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
    let sink_entry_id = entry_id.clone();
    let sink_device_id = runtime.device_id.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        let chat = sink_chat.clone();
        let entry_id = sink_entry_id.clone();
        let device_id = sink_device_id.clone();
        Box::pin(async move {
            match event {
                AgentEvent::MessageStart { message }
                | AgentEvent::MessageUpdate { message, .. } => {
                    if matches!(&*message, AgentMessage::Assistant(_)) {
                        update_assistant_entry(
                            &chat,
                            &entry_id,
                            &message,
                            MessageStatus::Streaming,
                            &device_id,
                        );
                    }
                }
                AgentEvent::MessageEnd { message } => {
                    if matches!(&*message, AgentMessage::Assistant(_)) {
                        update_assistant_entry(
                            &chat,
                            &entry_id,
                            &message,
                            MessageStatus::Complete,
                            &device_id,
                        );
                    }
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
                "You are a coding assistant working in {cwd}. This runtime currently exposes no tools."
            ),
            messages: history.clone(),
            tools: None,
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

    #[test]
    fn assistant_message_maps_text_and_reasoning_to_doc_parts() {
        use pi_core::ai::types::{AssistantMessage, TextContent, ThinkingContent};

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
}
