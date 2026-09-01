//! holt-engine — the backend slot for the desktop shell.
//!
//! The original engine (agent orchestration, doc host, sync, auth, terminals,
//! repos, uploads) was removed when this repo became a UI shell. What remains:
//!
//! - [`StubEngine`] — an [`RpcService`] answering the RPC surface the UI speaks
//!   with empty catalogs, empty watch streams, and unknown-method errors for
//!   anything it has no backend for. It is
//!   the placeholder a real backend (e.g. pi-core-rs) replaces: implement the
//!   methods in [`StubEngine::handle`], keep the reply shapes, and the whole UI
//!   lights up without further changes.
//! - [`InstanceLock`] — single-instance guard on the data dir.
//! - provider discovery and Holt-owned credential storage behind the RPC seam.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandPayload, SessionMessageEntry,
    TranscriptFrame,
};
use holt_proto::{
    AuthState, Chat, ChatConfig, Device, DriveEntry, DriveListing, FolderEntry, FolderListing,
    ReasoningLevel, Session, SessionStatus, Space,
};
pub use holt_proto::{EngineInfo, WorkspaceScope};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::{
    agent::{
        agent_loop::{AgentEventSink, pass_through_llm_messages, run_agent_loop},
        types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage},
    },
    ai::{
        auth::types::CredentialStore,
        compat,
        types::{
            AssistantContent, Context as PiContext, Model as PiModel, RoleUser,
            SimpleStreamOptions, ThinkingLevel as ProviderThinkingLevel, UserContent, UserMessage,
        },
    },
};
use serde::Deserialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub mod credentials;
pub mod instance_lock;
pub mod provider_settings;
pub mod providers;
mod store;

use credentials::HoltCredentialStore;
pub use instance_lock::InstanceLock;
use provider_settings::ProviderSettingsStore;
use providers::ProviderAdapter;
use store::{load_chats, load_or_create_device_id, load_spaces, persist_chats, persist_spaces};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Everything the stub backend needs. A real engine grows this back
/// (provider config, IPC port, …) as it needs it.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.holt`).
    pub data_dir: PathBuf,
}

/// The no-op backend. Serves the RPC method surface with empty data so the
/// shell boots with local catalogs and empty workspace data.
pub struct StubEngine {
    engine_info: EngineInfo,
    data_dir: PathBuf,
    spaces: RwLock<Vec<Space>>,
    spaces_tx: watch::Sender<serde_json::Value>,
    runtime: Arc<AgentRuntime>,
    providers: Arc<ProviderAdapter>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

struct ChatRuntime {
    transcript: RwLock<Vec<SessionMessageEntry>>,
    history: RwLock<Vec<AgentMessage>>,
    transcript_tx: watch::Sender<serde_json::Value>,
    cancel: Mutex<Option<CancellationToken>>,
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

    fn publish(&self) {
        let transcript = self.transcript.read().unwrap_or_else(|e| e.into_inner());
        if let Ok(value) = serde_json::to_value(TranscriptFrame::reset(&transcript)) {
            self.transcript_tx.send_replace(value);
        }
    }
}

struct AgentRuntime {
    device_id: String,
    chats: RwLock<Vec<Chat>>,
    chats_tx: watch::Sender<serde_json::Value>,
    sessions: RwLock<Vec<Session>>,
    sessions_tx: watch::Sender<serde_json::Value>,
    chat_runtime: Mutex<HashMap<String, Arc<ChatRuntime>>>,
}

impl AgentRuntime {
    fn new(device_id: String, chats: Vec<Chat>) -> Self {
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

    fn chat(&self, chat_id: &str) -> Arc<ChatRuntime> {
        let mut chats = self.chat_runtime.lock().unwrap_or_else(|e| e.into_inner());
        chats
            .entry(chat_id.to_string())
            .or_insert_with(|| Arc::new(ChatRuntime::new()))
            .clone()
    }

    fn publish_chats(&self) {
        let chats = self.chats.read().unwrap_or_else(|e| e.into_inner());
        if let Ok(value) = serde_json::to_value(&*chats) {
            self.chats_tx.send_replace(value);
        }
    }

    fn set_session(&self, chat_id: &str, status: SessionStatus) {
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

impl StubEngine {
    /// Assemble the stub against a data dir. Takes the instance lock and
    /// resolves a stable device id.
    pub fn assemble(config: &EngineConfig) -> Result<Self, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let device_id = load_or_create_device_id(&config.data_dir)?;
        let spaces = load_spaces(&config.data_dir)?;
        let spaces_value =
            serde_json::to_value(&spaces).map_err(|error| EngineError::Other(error.to_string()))?;
        let (spaces_tx, _) = watch::channel(spaces_value);
        let runtime = Arc::new(AgentRuntime::new(
            device_id.clone(),
            load_chats(&config.data_dir)?,
        ));
        let credentials = Arc::new(HoltCredentialStore::load(&config.data_dir)?);
        let provider_settings = Arc::new(ProviderSettingsStore::load(&config.data_dir)?);
        let providers = Arc::new(ProviderAdapter::new(credentials, provider_settings));
        Ok(Self {
            engine_info: EngineInfo {
                device_id,
                workspace_scope: WorkspaceScope::Local,
            },
            data_dir: config.data_dir.clone(),
            spaces: RwLock::new(spaces),
            spaces_tx,
            runtime,
            providers,
            _instance_lock: lock,
        })
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    fn watch_spaces(&self) -> RpcReply {
        let receiver = self.spaces_tx.subscribe();
        let stream =
            futures::stream::unfold((receiver, true), |(mut receiver, first)| async move {
                if !first && receiver.changed().await.is_err() {
                    return None;
                }
                let value = receiver.borrow().clone();
                Some((value, (receiver, false)))
            });
        RpcReply::Stream(Box::pin(stream))
    }

    fn watch_value(receiver: watch::Receiver<serde_json::Value>) -> RpcReply {
        let stream =
            futures::stream::unfold((receiver, true), |(mut receiver, first)| async move {
                if !first && receiver.changed().await.is_err() {
                    return None;
                }
                let value = receiver.borrow().clone();
                Some((value, (receiver, false)))
            });
        RpcReply::Stream(Box::pin(stream))
    }

    fn create_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: CreateSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.space_id.trim().is_empty()
            || params.device_id.trim().is_empty()
            || params.path.trim().is_empty()
        {
            return Err(RpcError::BadParams(
                "spaceId, deviceId, and path must not be empty".into(),
            ));
        }

        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        if spaces.iter().any(|space| {
            space.id == params.space_id
                || (space.device_id == params.device_id && space.path == params.path)
        }) {
            return RpcReply::value(&serde_json::json!({}));
        }
        spaces.push(Space {
            id: params.space_id,
            device_id: params.device_id,
            path: params.path,
            name: None,
            git_detected: params.git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        });
        persist_spaces(&self.data_dir, &spaces).map_err(|error| {
            spaces.pop();
            RpcError::Failed(error.to_string())
        })?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        self.spaces_tx.send_replace(value);
        RpcReply::value(&serde_json::json!({}))
    }

    fn create_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: CreateChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let space = params.space_id.as_deref().and_then(|space_id| {
            self.spaces
                .read()
                .ok()?
                .iter()
                .find(|space| space.id == space_id)
                .cloned()
        });
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if chats.iter().any(|chat| chat.id == params.chat_id) {
            return RpcReply::value(&serde_json::json!({}));
        }
        chats.push(Chat {
            id: params.chat_id.clone(),
            device_id: params
                .device_id
                .or_else(|| space.as_ref().map(|space| space.device_id.clone()))
                .unwrap_or_else(|| self.engine_info.device_id.clone()),
            title: None,
            archived: false,
            cwd: params
                .cwd
                .or_else(|| space.as_ref().map(|space| space.path.clone())),
            branch: params.branch,
            checkout_id: space.as_ref().and_then(|space| space.checkout_id.clone()),
            source_context: None,
            config: params.config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            space_id: params.space_id,
            last_seen_at: None,
            room_gen: None,
        });
        drop(chats);
        persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.chat(&params.chat_id);
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    async fn queue_command(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: QueueCommandParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let chat = self.runtime.chat(&params.chat_id);
        match params.command {
            SessionCommandPayload::Interrupt {} => {
                if let Some(cancel) = chat
                    .cancel
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    cancel.cancel();
                }
            }
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                let Some(api_key) = self
                    .providers
                    .credentials
                    .reveal_key(request.provider.as_str())
                    .await
                else {
                    return Err(RpcError::Failed(format!(
                        "provider {} is not configured",
                        request.provider
                    )));
                };
                let model = self
                    .providers
                    .resolve_model(request.provider.as_str(), &request.model)
                    .map_err(RpcError::BadParams)?;
                let mut active = chat.cancel.lock().unwrap_or_else(|e| e.into_inner());
                if active.as_ref().is_some_and(|token| !token.is_cancelled()) {
                    return Err(RpcError::Failed("this chat is already running".into()));
                }
                let cancel = CancellationToken::new();
                *active = Some(cancel.clone());
                drop(active);

                let now = Utc::now();
                let timestamp = now.timestamp_millis();
                chat.transcript
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(SessionMessageEntry {
                        id: message_id,
                        role: MessageRole::User,
                        parts: vec![MessagePart::Text {
                            id: "t0".into(),
                            text: request.prompt.clone(),
                        }],
                        created_at: timestamp,
                        device_id: self.engine_info.device_id.clone(),
                        status: None,
                        continuation_of: None,
                    });
                chat.publish();

                {
                    let mut chats = self
                        .runtime
                        .chats
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) {
                        row.cwd = Some(request.cwd.clone());
                        row.config = Some(ChatConfig {
                            provider: request.provider.clone(),
                            model: request.model.clone(),
                            reasoning: request.reasoning,
                            model_options: request.model_options.clone(),
                            sandbox: request.sandbox,
                        });
                        row.last_message_preview = Some(request.prompt.chars().take(120).collect());
                        row.last_message_at = Some(now);
                        if row.title.is_none() {
                            row.title = Some(
                                request
                                    .prompt
                                    .lines()
                                    .next()
                                    .unwrap_or("New chat")
                                    .chars()
                                    .take(60)
                                    .collect(),
                            );
                        }
                    }
                }
                persist_chats(
                    &self.data_dir,
                    &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
                )
                .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.runtime.publish_chats();
                self.runtime
                    .set_session(&params.chat_id, SessionStatus::Working);

                let runtime = self.runtime.clone();
                let chat_id = params.chat_id;
                tokio::spawn(run_agent_command(AgentRun {
                    runtime,
                    chat_id,
                    chat,
                    prompt: request.prompt,
                    cwd: request.cwd,
                    reasoning: request.reasoning,
                    model,
                    api_key,
                    timestamp,
                    cancel,
                }));
            }
            SessionCommandPayload::Steer { .. } => {
                return Err(RpcError::Failed("steering is not available yet".into()));
            }
            SessionCommandPayload::RespondInput { .. } => {
                return Err(RpcError::Failed(
                    "input responses are not available yet".into(),
                ));
            }
        }
        RpcReply::value(&serde_json::json!({}))
    }

    fn set_chat_config(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let config: ChatConfig = serde_json::from_value(
            params
                .get("config")
                .cloned()
                .ok_or_else(|| RpcError::BadParams("config is required".into()))?,
        )
        .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let chat = chats
            .iter_mut()
            .find(|chat| chat.id == chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        chat.config = Some(config);
        persist_chats(&self.data_dir, &chats)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(chats);
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSpaceParams {
    space_id: String,
    device_id: String,
    path: String,
    #[serde(default)]
    git_detected: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChatParams {
    chat_id: String,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    config: Option<ChatConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
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

fn required_string<'a>(params: &'a serde_json::Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{field} is required")))
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

struct AgentRun {
    runtime: Arc<AgentRuntime>,
    chat_id: String,
    chat: Arc<ChatRuntime>,
    prompt: String,
    cwd: String,
    reasoning: Option<ReasoningLevel>,
    model: PiModel,
    api_key: String,
    timestamp: i64,
    cancel: CancellationToken,
}

async fn run_agent_command(run: AgentRun) {
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

/// A watch stream that emits `value` once, then stays open (never changes).
fn static_watch(value: serde_json::Value) -> RpcReply {
    use futures::StreamExt;
    RpcReply::Stream(
        futures::stream::once(futures::future::ready(value))
            .chain(futures::stream::pending::<serde_json::Value>())
            .boxed(),
    )
}

/// A stream that never emits — for subscriptions the stub has no data for.
fn pending_stream() -> RpcReply {
    use futures::StreamExt;
    RpcReply::Stream(futures::stream::pending::<serde_json::Value>().boxed())
}

/// The local machine as a device row — enough for the add-space palette to
/// browse this computer (presence for the local device is always "online").
fn local_device(device_id: &str) -> Device {
    Device {
        id: device_id.to_string(),
        name: hostname(),
        platform: std::env::consts::OS.to_string(),
        last_seen_at: None,
        created_at: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
}

/// Bare hostname without any domain suffix ("macbook.local" → "macbook").
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]);
        let name = name.split('.').next().unwrap_or("").trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "This device".to_string()
}

fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .map(|home| home.to_string_lossy().to_string())
        .filter(|home| !home.is_empty())
}

/// The UI expands `~` itself; tolerate it here anyway so the method is
/// callable without the shell's helpers.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Browse cap — the palette scrolls, but a runaway directory (think `/`) is
/// still bounded; `truncated` tells the UI some entries were dropped.
const FOLDER_ENTRY_CAP: usize = 500;

fn list_folders(params: &serde_json::Value) -> Result<FolderListing, String> {
    let requested = params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let path = match requested {
        Some(path) => expand_tilde(path),
        None => home_dir().ok_or_else(|| "could not resolve your home folder".to_string())?,
    };
    let read =
        std::fs::read_dir(&path).map_err(|error| format!("could not read that folder: {error}"))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().to_string();
        // Dotfiles stay hidden — the browser is for project folders.
        if name.starts_with('.') {
            continue;
        }
        // `std::fs::metadata` (not `item.metadata`) so a symlinked folder
        // still counts as a folder.
        let Ok(meta) = std::fs::metadata(item.path()) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if entries.len() >= FOLDER_ENTRY_CAP {
            truncated = true;
            break;
        }
        entries.push(FolderEntry {
            is_repo: item.path().join(".git").exists(),
            name,
            is_dir: true,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(FolderListing {
        path,
        entries,
        truncated,
    })
}

/// Browse roots beyond home: the system root plus mounted volumes. macOS
/// keeps mounts under `/Volumes`; the boot volume's alias there resolves to
/// `/`, so it is deduped against the System row.
fn list_drives() -> DriveListing {
    let mut drives = vec![DriveEntry {
        name: "System".to_string(),
        path: "/".to_string(),
    }];
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for item in read.flatten() {
            let path = item.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            if std::fs::canonicalize(&path).is_ok_and(|canon| canon == Path::new("/")) {
                continue;
            }
            drives.push(DriveEntry {
                name: item.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
    DriveListing { drives }
}

#[async_trait]
impl RpcService for StubEngine {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => RpcReply::value(&serde_json::json!({ "ready": true })),
            methods::LOCAL_DEVICE => RpcReply::value(&serde_json::json!({
                "deviceId": self.engine_info.device_id,
            })),
            methods::LIST_PROVIDERS => RpcReply::value(&self.providers.providers().await),
            methods::SAVE_PROVIDER_KEY => {
                let provider = required_string(&params, "providerId")?;
                let key = required_string(&params, "key")?;
                if !ProviderAdapter::is_eligible(provider) {
                    return Err(RpcError::BadParams(
                        "unknown or unsupported provider".into(),
                    ));
                }
                self.providers
                    .credentials
                    .save_key(provider, key)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REVEAL_PROVIDER_KEY => {
                let provider = required_string(&params, "providerId")?;
                RpcReply::value(&serde_json::json!({
                    "key": self.providers.credentials.reveal_key(provider).await
                }))
            }
            methods::REMOVE_PROVIDER_KEY => {
                let provider = required_string(&params, "providerId")?;
                self.providers
                    .credentials
                    .delete(provider, None)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::ADD_PROVIDER_MODEL => {
                let provider = required_string(&params, "providerId")?;
                let submitted = required_string(&params, "modelId")?.trim();
                let qualified_prefix = format!("{provider}/");
                let model = submitted
                    .strip_prefix(&qualified_prefix)
                    .unwrap_or(submitted);
                if !ProviderAdapter::is_eligible(provider) {
                    return Err(RpcError::BadParams(
                        "unknown or unsupported provider".into(),
                    ));
                }
                if !self.providers.can_add_custom_model(provider) {
                    return Err(RpcError::BadParams(
                        "provider has no model template for custom IDs".into(),
                    ));
                }
                self.providers
                    .settings
                    .add_custom_model(provider, model)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::LIST_MODELS => {
                let provider = required_string(&params, "providerId")?;
                RpcReply::value(&self.providers.models_for(provider))
            }
            methods::LIST_COMMANDS => RpcReply::value(&serde_json::json!([])),
            // Local folder browsing for the add-space palette. The UI only
            // targets remote devices over the relay; here every browse is local.
            methods::LIST_FOLDERS => match list_folders(&params) {
                Ok(listing) => RpcReply::value(&listing),
                Err(message) => Err(RpcError::Failed(message)),
            },
            methods::LIST_DRIVES => RpcReply::value(&list_drives()),

            // Entity watches: one snapshot, then silence. Devices are real —
            // the local machine browses its own folders — the rest stay empty.
            methods::WATCH_DEVICES => {
                let value = serde_json::to_value(vec![local_device(&self.engine_info.device_id)])
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }
            methods::WATCH_CHATS => Ok(Self::watch_value(self.runtime.chats_tx.subscribe())),
            methods::WATCH_SESSIONS => Ok(Self::watch_value(self.runtime.sessions_tx.subscribe())),
            methods::WATCH_TRANSFERS => Ok(static_watch(serde_json::json!([]))),
            methods::WATCH_SPACES => Ok(self.watch_spaces()),
            methods::WATCH_CONNECTIVITY => {
                // Default = state Disabled ("no edge transports on this
                // profile — hide the pill"), no chat rooms.
                let value = serde_json::to_value(holt_proto::Connectivity::default())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }
            methods::AUTH_STATUS => {
                let value = serde_json::to_value(AuthState::SignedOut)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }

            // Transcript / terminal data: nothing to serve.
            methods::WATCH_DOC_MESSAGES => {
                let chat_id = params
                    .get("chatId")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| RpcError::BadParams("chatId is required".into()))?;
                let chat = self.runtime.chat(chat_id);
                Ok(Self::watch_value(chat.transcript_tx.subscribe()))
            }
            methods::SUBSCRIBE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::WATCH_CHECKOUT_CHANGE_REQUEST => Ok(pending_stream()),

            // No-op liveness pokes the UI fires defensively.
            methods::PROBE_SYNC => RpcReply::value(&serde_json::json!({})),

            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("createSpace") =>
            {
                self.create_space(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("createChat") =>
            {
                self.create_chat(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("setChatConfig") =>
            {
                self.set_chat_config(params)
            }
            methods::QUEUE_COMMAND => self.queue_command(params).await,

            // Everything the stub has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_across_assembles() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let first = StubEngine::assemble(&config).unwrap();
        let id = first.engine_info().device_id.clone();
        // The lock dies with the engine; a fresh assemble reads the same id.
        drop(first);
        let second = StubEngine::assemble(&config).unwrap();
        assert_eq!(second.engine_info().device_id, id);
    }

    #[test]
    fn second_engine_on_one_data_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let _first = StubEngine::assemble(&config).unwrap();
        assert!(StubEngine::assemble(&config).is_err());
    }

    #[test]
    fn list_folders_returns_dirs_sorted_and_marks_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("zed")).unwrap();
        std::fs::create_dir(root.join("Alpha")).unwrap();
        std::fs::create_dir(root.join("repo")).unwrap();
        std::fs::create_dir(root.join("repo/.git")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("notes.txt"), "hi").unwrap();
        let listing = list_folders(&serde_json::json!({
            "path": root.to_string_lossy(),
        }))
        .unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "repo", "zed"]);
        assert!(listing.entries.iter().all(|e| e.is_dir));
        assert!(listing.entries[1].is_repo);
        assert!(!listing.entries[0].is_repo);
        assert!(!listing.truncated);
    }

    #[test]
    fn list_folders_errors_read_like_folder_failures() {
        // The UI distinguishes folder-level failures by the word "folder".
        let error =
            list_folders(&serde_json::json!({ "path": "/definitely/not/here" })).unwrap_err();
        assert!(error.contains("folder"), "unexpected message: {error}");
    }

    #[test]
    fn expand_tilde_resolves_against_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/dev"), format!("{home}/dev"));
        assert_eq!(expand_tilde("/abs"), "/abs");
    }

    #[test]
    fn list_drives_always_offers_the_system_root() {
        let drives = list_drives();
        assert!(drives.drives.iter().any(|d| d.path == "/"));
        // The boot volume's /Volumes alias must not duplicate the System row.
        assert!(
            !drives.drives.iter().any(|d| d.path != "/"
                && std::fs::canonicalize(&d.path).is_ok_and(|p| p == Path::new("/")))
        );
    }

    #[tokio::test]
    async fn create_space_updates_watch_and_survives_restart() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut spaces) = engine
            .handle(methods::WATCH_SPACES, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchSpaces did not return a stream");
        };
        assert_eq!(spaces.next().await.unwrap(), serde_json::json!([]));

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createSpace",
                    "spaceId": "space-1",
                    "deviceId": engine.engine_info().device_id,
                    "path": "/tmp/project",
                    "gitDetected": true,
                }),
            )
            .await
            .unwrap();

        let update = spaces.next().await.unwrap();
        assert_eq!(update.as_array().unwrap().len(), 1);
        assert_eq!(update[0]["id"], "space-1");
        assert_eq!(update[0]["path"], "/tmp/project");
        drop(spaces);
        drop(engine);

        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut spaces) = engine
            .handle(methods::WATCH_SPACES, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchSpaces did not return a stream");
        };
        let restored = spaces.next().await.unwrap();
        assert_eq!(restored.as_array().unwrap().len(), 1);
        assert_eq!(restored[0]["id"], "space-1");
    }

    #[test]
    fn provider_catalog_uses_provider_qualified_model_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HoltCredentialStore::load(dir.path()).unwrap());
        let settings = Arc::new(ProviderSettingsStore::load(dir.path()).unwrap());
        let models = ProviderAdapter::new(store, settings).models_for("openai");
        assert!(!models.is_empty());
        assert!(models.iter().all(|model| model.id.contains('/')));
        assert!(models.iter().any(|model| model.id == "openai/gpt-5.4"));
    }

    #[tokio::test]
    async fn custom_provider_model_is_listed_resolved_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().into(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        engine
            .handle(
                methods::ADD_PROVIDER_MODEL,
                serde_json::json!({
                    "providerId": "openai",
                    "modelId": "openai/gpt-private-2026-09-01"
                }),
            )
            .await
            .unwrap();

        let custom_id = "openai/gpt-private-2026-09-01";
        let RpcReply::Value(models) = engine
            .handle(
                methods::LIST_MODELS,
                serde_json::json!({"providerId": "openai"}),
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(models.as_array().unwrap().iter().any(|model| {
            model["id"] == custom_id && model["label"] == "gpt-private-2026-09-01"
        }));
        assert_eq!(
            engine
                .providers
                .resolve_model("openai", custom_id)
                .unwrap()
                .id,
            "gpt-private-2026-09-01"
        );
        drop(engine);

        let restored = StubEngine::assemble(&config).unwrap();
        assert!(
            restored
                .providers
                .models_for("openai")
                .iter()
                .any(|model| model.id == custom_id)
        );
    }

    #[tokio::test]
    async fn provider_credential_rpc_keeps_secrets_out_of_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StubEngine::assemble(&EngineConfig {
            data_dir: dir.path().into(),
        })
        .unwrap();
        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({
                    "providerId": "openai",
                    "key": "rpc-test-secret"
                }),
            )
            .await
            .unwrap();

        let RpcReply::Value(providers) = engine
            .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(!providers.to_string().contains("rpc-test-secret"));
        assert!(
            providers
                .as_array()
                .unwrap()
                .iter()
                .any(|provider| provider["id"] == "openai" && provider["configured"] == true)
        );

        let RpcReply::Value(models) = engine
            .handle(
                methods::LIST_MODELS,
                serde_json::json!({"providerId": "openai"}),
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(
            models
                .as_array()
                .unwrap()
                .iter()
                .all(|model| model["provider"] == "openai"
                    && model["id"].as_str().unwrap().starts_with("openai/"))
        );
        assert!(!models.to_string().contains("rpc-test-secret"));

        let RpcReply::Value(revealed) = engine
            .handle(
                methods::REVEAL_PROVIDER_KEY,
                serde_json::json!({"providerId": "openai"}),
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(revealed["key"], "rpc-test-secret");
        engine
            .handle(
                methods::REMOVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "openai"}),
            )
            .await
            .unwrap();
        let RpcReply::Value(providers) = engine
            .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(
            providers
                .as_array()
                .unwrap()
                .iter()
                .any(|provider| provider["id"] == "openai" && provider["configured"] == false)
        );
    }

    #[tokio::test]
    async fn provider_catalog_groups_variants_by_organization() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StubEngine::assemble(&EngineConfig {
            data_dir: dir.path().into(),
        })
        .unwrap();
        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "minimax-cn", "key": "secret"}),
            )
            .await
            .unwrap();

        let RpcReply::Value(providers) = engine
            .handle(methods::LIST_PROVIDERS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(
            !providers.to_string().contains("secret"),
            "catalog leaks credentials"
        );
        let providers = providers.as_array().unwrap();
        let minimax = providers
            .iter()
            .find(|row| row["id"] == "minimax")
            .expect("minimax organization row missing");
        assert_eq!(minimax["configured"], true);
        let variants: Vec<&str> = minimax["variants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| variant["id"].as_str().unwrap())
            .collect();
        assert_eq!(variants, ["minimax", "minimax-cn"]);
        assert!(
            minimax["variants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["id"] == "minimax-cn" && variant["configured"] == true)
        );
        assert!(
            minimax["variants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["id"] == "minimax" && variant["configured"] == false)
        );
    }

    #[tokio::test]
    async fn run_against_unconfigured_variant_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().into(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "minimax-cn", "key": "secret"}),
            )
            .await
            .unwrap();

        let run = |provider: &'static str| {
            serde_json::json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "run",
                    "messageId": "message-1",
                    "request": {
                        "prompt": "hello",
                        "provider": provider,
                        "model": format!("{provider}/MiniMax-M2"),
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": dir.path(),
                        "sandbox": "workspace-write"
                    }
                }
            })
        };
        // Sibling variant configured; the international one has no key.
        let error = match engine.handle(methods::QUEUE_COMMAND, run("minimax")).await {
            Err(error) => error,
            Ok(_) => panic!("unconfigured variant accepted a run"),
        };
        assert!(error.to_string().contains("not configured"));

        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "minimax", "key": "secret-2"}),
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .providers
                .credentials
                .reveal_key("minimax")
                .await
                .as_deref(),
            Some("secret-2")
        );
        drop(engine);

        // Keys are per-variant and survive restart.
        let restored = StubEngine::assemble(&config).unwrap();
        assert_eq!(
            restored
                .providers
                .credentials
                .reveal_key("minimax-cn")
                .await
                .as_deref(),
            Some("secret")
        );
    }

    #[tokio::test]
    async fn removing_a_key_preserves_persisted_chat_selection() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().into(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "openai", "key": "secret"}),
            )
            .await
            .unwrap();
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createChat",
                    "chatId": "chat-1",
                    "config": {
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": "high",
                        "modelOptions": {},
                        "sandbox": "workspace-write"
                    }
                }),
            )
            .await
            .unwrap();
        engine
            .handle(
                methods::REMOVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "openai"}),
            )
            .await
            .unwrap();
        drop(engine);

        let engine = StubEngine::assemble(&config).unwrap();
        let chats = engine.runtime.chats.read().unwrap();
        let selection = chats[0].config.as_ref().unwrap();
        assert_eq!(selection.provider.as_str(), "openai");
        assert_eq!(selection.model, "openai/gpt-5.4");
        drop(chats);
        assert!(
            engine
                .providers
                .credentials
                .reveal_key("openai")
                .await
                .is_none()
        );
    }

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

    #[tokio::test]
    async fn create_chat_updates_chat_and_transcript_watches() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let engine = StubEngine::assemble(&EngineConfig {
            data_dir: dir.path().to_path_buf(),
        })
        .unwrap();
        let RpcReply::Stream(mut chats) = engine
            .handle(methods::WATCH_CHATS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchChats did not return a stream");
        };
        assert_eq!(chats.next().await.unwrap(), serde_json::json!([]));

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createChat",
                    "chatId": "chat-1",
                    "deviceId": engine.engine_info().device_id,
                }),
            )
            .await
            .unwrap();
        assert_eq!(chats.next().await.unwrap()[0]["id"], "chat-1");

        let RpcReply::Stream(mut transcript) = engine
            .handle(
                methods::WATCH_DOC_MESSAGES,
                serde_json::json!({ "chatId": "chat-1" }),
            )
            .await
            .unwrap()
        else {
            panic!("WatchDocMessages did not return a stream");
        };
        assert_eq!(
            transcript.next().await.unwrap(),
            serde_json::json!({ "reset": [] })
        );
    }
}
