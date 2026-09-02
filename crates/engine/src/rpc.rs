//! The RPC surface: `RpcService` dispatch plus the space/chat mutation and
//! queue-command handlers it routes to.

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry, TranscriptFrame,
    diff_transcript,
};
use holt_proto::{AuthState, Chat, ChatConfig, SessionStatus, Space};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::StubEngine;
use crate::agent::{AgentRun, ChatRuntime, run_agent_command};
use crate::local_fs::{list_drives, list_folders, local_device};
use crate::providers::ProviderAdapter;
use crate::store::{persist_chats, persist_spaces};

impl StubEngine {
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

    /// Per-subscriber delta stream for `WatchDocMessages`: the chat watch
    /// carries the current transcript snapshot, and each subscriber diffs it
    /// against its own baseline, so a fresh subscription opens with a full
    /// reset and streaming ticks carry only the changed entry. Forwarding the
    /// engine's raw snapshot as a whole-transcript `reset` per publish
    /// re-serialized (and re-parsed) megabytes per delta token on long chats —
    /// enough to stall the stream pump and deliver a finished reply in one
    /// lump instead of streaming it.
    fn watch_transcript(chat: Arc<ChatRuntime>) -> RpcReply {
        let stream = futures::stream::unfold(
            (
                chat.transcript_tx.subscribe(),
                None::<Arc<Vec<SessionMessageEntry>>>,
                true,
            ),
            |(mut receiver, mut baseline, mut first)| async move {
                loop {
                    if !first && receiver.changed().await.is_err() {
                        return None;
                    }
                    first = false;
                    let current = receiver.borrow_and_update().clone();
                    let opening = baseline.is_none();
                    let frame = match &baseline {
                        None => TranscriptFrame::reset(current.as_slice()),
                        Some(prev) => diff_transcript(prev, &current),
                    };
                    baseline = Some(current);
                    if !opening && frame.is_empty_delta() {
                        continue;
                    }
                    match serde_json::to_value(&frame) {
                        Ok(value) => {
                            return Some((value, (receiver, baseline, false)));
                        }
                        Err(_) => continue,
                    }
                }
            },
        );
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

    fn set_chat_archived(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: SetChatArchivedParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) else {
            // Unknown chat: idempotent no-op, matching create's duplicate path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.archived = params.archived;
        drop(chats);
        persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    fn delete_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: DeleteChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let before = chats.len();
        chats.retain(|chat| chat.id != params.chat_id);
        let removed = chats.len() != before;
        drop(chats);
        if removed {
            persist_chats(
                &self.data_dir,
                &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
            )
            .map_err(|error| RpcError::Failed(error.to_string()))?;
            self.runtime.remove_chat(&params.chat_id);
            self.runtime.publish_chats();
        }
        // Unknown chat: idempotent no-op, matching the archive path.
        RpcReply::value(&serde_json::json!({}))
    }

    fn mark_chat_seen(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) else {
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        // Only an unseen row needs a write: re-marks (racing devices, a
        // re-selected row) must not republish the chat list.
        if !row.unseen() {
            return RpcReply::value(&serde_json::json!({}));
        }
        row.last_seen_at = Some(Utc::now());
        drop(chats);
        persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .map_err(|error| RpcError::Failed(error.to_string()))?;
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
struct SetChatArchivedParams {
    chat_id: String,
    archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteChatParams {
    chat_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

fn required_string<'a>(params: &'a serde_json::Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{field} is required")))
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
                if self.providers.has_model(provider, model) {
                    return Err(RpcError::Failed(
                        "This model ID already exists in this provider's model list".into(),
                    ));
                }
                self.providers
                    .settings
                    .add_custom_model(provider, model)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REMOVE_PROVIDER_MODEL => {
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
                self.providers
                    .settings
                    .remove_custom_model(provider, model)
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

            // Git capability (ADR-0001): branch listing and safe switching
            // for space folders. Errors carry git's own message — the picker
            // renders it in place.
            methods::LIST_REFS => {
                let repo_path = required_string(&params, "repoPath")?;
                let refs = self
                    .git
                    .list_refs(repo_path)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&refs)
            }
            methods::LIST_BRANCHES => {
                let repo_path = required_string(&params, "repoPath")?;
                let branches = self
                    .git
                    .list_branches(repo_path)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&branches)
            }
            methods::SWITCH_REF => {
                let repo_path = required_string(&params, "repoPath")?;
                let ref_name = required_string(&params, "refName")?;
                self.git
                    .switch_ref(repo_path, ref_name)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&serde_json::json!({}))
            }

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
                Ok(Self::watch_transcript(chat))
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
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("setChatArchived") =>
            {
                self.set_chat_archived(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("deleteChat") =>
            {
                self.delete_chat(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("markChatSeen") =>
            {
                self.mark_chat_seen(params)
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
