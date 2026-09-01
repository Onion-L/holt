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
    path::PathBuf,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use holt_proto::{AuthState, Chat, ChatConfig, SessionStatus, Space};
pub use holt_proto::{EngineInfo, WorkspaceScope};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use serde::Deserialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

mod agent;
pub mod credentials;
pub mod instance_lock;
mod local_fs;
pub mod provider_settings;
pub mod providers;
mod store;

use agent::{AgentRun, AgentRuntime, run_agent_command};
use credentials::HoltCredentialStore;
pub use instance_lock::InstanceLock;
use local_fs::{list_drives, list_folders, local_device};
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
