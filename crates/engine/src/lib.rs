//! holt-engine — the in-process backend for the desktop shell.
//!
//! - [`StubEngine`] — the [`RpcService`] the UI speaks to over the
//!   in-memory RPC transport: space/chat persistence, watch streams,
//!   provider discovery and credentials, folder browsing, and a
//!   single-agent LLM run loop over pi-core. A real backend replaces it
//!   behind the same trait: implement the methods in its `handle`, keep
//!   the reply shapes, and the whole UI keeps working.
//! - [`InstanceLock`] — single-instance guard on the data dir.
//! - module map: `agent` (run loop + runtime state), `rpc` (dispatch +
//!   handlers), `store` (JSON persistence), `local_fs` (folder browsing),
//!   plus provider discovery and Holt-owned credential storage behind the
//!   RPC seam.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use holt_proto::Space;
pub use holt_proto::{EngineInfo, WorkspaceScope};
use tokio::sync::watch;

mod agent;
pub mod credentials;
pub mod instance_lock;
mod local_fs;
pub mod provider_settings;
pub mod providers;
mod rpc;
mod store;
mod tools;

use agent::AgentRuntime;
use credentials::HoltCredentialStore;
pub use instance_lock::InstanceLock;
use provider_settings::ProviderSettingsStore;
use providers::ProviderAdapter;
use store::{load_chats, load_or_create_device_id, load_spaces};

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
            config.data_dir.clone(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_rpc::{RpcReply, RpcService, methods};

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

    #[tokio::test]
    async fn set_chat_archived_flips_persists_and_survives_restart() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
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
                serde_json::json!({ "op": "createChat", "chatId": "chat-1" }),
            )
            .await
            .unwrap();
        chats.next().await.unwrap();

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "setChatArchived",
                    "chatId": "chat-1",
                    "archived": true,
                }),
            )
            .await
            .unwrap();
        let frame = chats.next().await.unwrap();
        assert_eq!(frame[0]["archived"], serde_json::json!(true));

        // Unknown chat is an idempotent no-op, not an error.
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "setChatArchived",
                    "chatId": "missing",
                    "archived": true,
                }),
            )
            .await
            .unwrap();
        drop(chats);
        drop(engine);

        // The flag persists: a fresh engine replays it, and unarchive
        // round-trips.
        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut chats) = engine
            .handle(methods::WATCH_CHATS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchChats did not return a stream");
        };
        let frame = chats.next().await.unwrap();
        assert_eq!(frame[0]["archived"], serde_json::json!(true));

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "setChatArchived",
                    "chatId": "chat-1",
                    "archived": false,
                }),
            )
            .await
            .unwrap();
        let frame = chats.next().await.unwrap();
        assert_eq!(frame[0]["archived"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn mark_chat_seen_stamps_persists_and_skips_already_seen() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
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
                serde_json::json!({ "op": "createChat", "chatId": "chat-1" }),
            )
            .await
            .unwrap();
        chats.next().await.unwrap();

        // The Done-badge precondition: a message newer than the (absent)
        // seen marker.
        engine.runtime.chats.write().unwrap()[0].last_message_at = Some(chrono::Utc::now());
        engine.runtime.publish_chats();
        let frame = chats.next().await.unwrap();
        assert_eq!(frame[0]["lastSeenAt"], serde_json::Value::Null);

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({ "op": "markChatSeen", "chatId": "chat-1" }),
            )
            .await
            .unwrap();
        let frame = chats.next().await.unwrap();
        let parse_stamp = |field: &str| -> chrono::DateTime<chrono::Utc> {
            serde_json::from_value(frame[0][field].clone())
                .unwrap_or_else(|error| panic!("{field} not an RFC3339 stamp: {error}"))
        };
        assert!(
            parse_stamp("lastSeenAt") >= parse_stamp("lastMessageAt"),
            "seen marker must clear unseen"
        );

        // Re-marking a seen chat neither moves the marker nor republishes.
        let probe = engine.runtime.chats_tx.subscribe();
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({ "op": "markChatSeen", "chatId": "chat-1" }),
            )
            .await
            .unwrap();
        assert!(!probe.has_changed().unwrap());

        // Unknown chat is an idempotent no-op, not an error.
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({ "op": "markChatSeen", "chatId": "missing" }),
            )
            .await
            .unwrap();
        drop(chats);
        drop(engine);

        // The marker persists: a fresh engine serves the chat as seen.
        let engine = StubEngine::assemble(&config).unwrap();
        let chats = engine.runtime.chats.read().unwrap();
        assert!(chats[0].last_seen_at.is_some());
    }

    #[tokio::test]
    async fn delete_chat_removes_persists_and_drops_transcript() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
        };
        let engine = StubEngine::assemble(&config).unwrap();
        let RpcReply::Stream(mut chats) = engine
            .handle(methods::WATCH_CHATS, serde_json::json!({}))
            .await
            .unwrap()
        else {
            panic!("WatchChats did not return a stream");
        };
        assert_eq!(chats.next().await.unwrap(), serde_json::json!([]));

        for chat_id in ["chat-1", "chat-2"] {
            engine
                .handle(
                    methods::MUTATE,
                    serde_json::json!({ "op": "createChat", "chatId": chat_id }),
                )
                .await
                .unwrap();
            chats.next().await.unwrap();
        }

        // A persisted transcript for chat-1 dies with the chat.
        crate::store::persist_transcript(
            dir.path(),
            "chat-1",
            &[holt_doc::SessionMessageEntry {
                id: "m1".into(),
                role: holt_doc::MessageRole::User,
                parts: vec![],
                created_at: 42,
                device_id: "device".into(),
                status: None,
                continuation_of: None,
            }],
        )
        .unwrap();
        assert!(dir.path().join("transcripts/chat-1.json").exists());

        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
            )
            .await
            .unwrap();
        let frame = chats.next().await.unwrap();
        let ids: Vec<&str> = frame
            .as_array()
            .unwrap()
            .iter()
            .map(|chat| chat["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["chat-2"]);
        assert!(!dir.path().join("transcripts/chat-1.json").exists());

        // Unknown chat is an idempotent no-op, not an error, with no frame.
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({ "op": "deleteChat", "chatId": "missing" }),
            )
            .await
            .unwrap();
        drop(chats);
        drop(engine);

        // The deletion persists: a fresh engine only knows chat-2.
        let engine = StubEngine::assemble(&config).unwrap();
        let chats = engine.runtime.chats.read().unwrap();
        let ids: Vec<&str> = chats.iter().map(|chat| chat.id.as_str()).collect();
        assert_eq!(ids, ["chat-2"]);
    }
}
