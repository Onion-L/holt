//! holt-engine — the in-process backend for the desktop shell.
//!
//! - [`LocalEngine`] — the [`RpcService`] the UI speaks to over the
//!   in-memory RPC transport: space/chat persistence, watch streams,
//!   provider discovery and credentials, folder browsing, and a
//!   single-agent LLM run loop over pi-core. Another backend can replace it
//!   behind the same trait: implement the methods in its `handle`, keep
//!   the reply shapes, and the whole UI keeps working.
//! - [`InstanceLock`] — single-instance guard on the data dir.
//! - module map: `agent` (run loop + runtime state), `rpc` (dispatch +
//!   handlers), `store` (JSON persistence), `history` (the per-chat
//!   model-facing History record, ADR-0010), `local_fs` (folder browsing),
//!   `path_search` (fuzzy workspace path search behind `SearchFiles`),
//!   `git` (the git2-backed branch/diff capability — the only git2 user),
//!   `skills` (the ADR-0005/0006 skill-root catalog), plus provider
//!   discovery and Holt-owned credential storage behind the RPC seam.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use holt_proto::Space;
pub use holt_proto::{EngineInfo, WorkspaceScope};
use tokio::sync::watch;

mod agent;
pub mod compaction;
pub mod credentials;
mod files;
mod gate;
mod git;
mod git_status_watch;
mod git_watch;
mod history;
pub mod images;
pub mod instance_lock;
mod local_fs;
mod mode_default;
mod path_search;
mod plan_mode;
pub mod provider_settings;
pub mod providers;
mod queue;
mod rpc;
mod skills;
mod store;
mod subagents;
mod terminals;
mod title_settings;
mod title_task;
mod tools;
mod trash;
mod turn_change_store;
mod turn_change_watch;
mod turn_changes;
mod turn_events;
mod web_search_settings;
mod workspace_watch;

use agent::AgentRuntime;
use credentials::HoltCredentialStore;
pub use instance_lock::InstanceLock;
use provider_settings::ProviderSettingsStore;
use providers::ProviderAdapter;
use store::{load_chats, load_or_create_device_id, load_spaces};
pub use title_task::{title_system_prompt, title_user_message};
pub use tools::{SearchBackend, SearchHit};

/// Maps a configured search-backend id (the `web-search.json` record's
/// `backend`) to its mounted adapter. The engine's built-in table fills in
/// as the backend slices land; tests inject one through `EngineConfig` to
/// script the `web_search` tool end to end.
pub type SearchBackendResolver = Arc<dyn Fn(&str) -> Option<Arc<dyn SearchBackend>> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Configuration for the local backend.
#[derive(Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.holt`).
    pub data_dir: PathBuf,
    /// Overrides the personal skill root (`~/.agents/skills` by default) —
    /// the engine's own (`<data_dir>/skills`) and the project root (from
    /// each chat's cwd) are unaffected. Tests pin temp dirs here so the
    /// three-root catalog is fixture-driven.
    pub personal_skills_dir: Option<PathBuf>,
    /// Injectable provider stream function, set only by tests (like
    /// `personal_skills_dir`): when present, every agent request goes
    /// through it instead of the built-in provider transport, so
    /// integration tests script model replies and assert on the message
    /// lists the "model" receives. Production assembly leaves it unset.
    pub stream_fn: Option<pi_core::agent::types::StreamFn>,
    /// Injectable search-backend resolver, set only by tests (like
    /// `stream_fn`): when present, it stands in for the built-in adapter
    /// table when the engine resolves the configured backend at Turn
    /// admission. Production assembly leaves it unset.
    pub search_backend_resolver: Option<SearchBackendResolver>,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The injected stream function and resolver are opaque closures;
        // their presence is the only fact worth printing.
        f.debug_struct("EngineConfig")
            .field("data_dir", &self.data_dir)
            .field("personal_skills_dir", &self.personal_skills_dir)
            .field("stream_fn", &self.stream_fn.as_ref().map(|_| "injected"))
            .field(
                "search_backend_resolver",
                &self.search_backend_resolver.as_ref().map(|_| "injected"),
            )
            .finish()
    }
}

/// The local backend. Serves the RPC method surface over the in-process
/// transport, including the agent loop, git capability, and skills catalog.
pub struct LocalEngine {
    service: EngineService,
    _instance_lock: InstanceLock,
}

/// Share execution services with queue workers without extending the public
/// engine's lifetime. Dropping LocalEngine stops workers before releasing its lock.
#[derive(Clone)]
struct EngineService {
    engine_info: EngineInfo,
    data_dir: PathBuf,
    spaces: Arc<RwLock<Vec<Space>>>,
    spaces_tx: watch::Sender<serde_json::Value>,
    runtime: Arc<AgentRuntime>,
    providers: Arc<ProviderAdapter>,
    git: git::Git,
    /// The `WatchCheckoutDiffs` hub — live checkout-diff awareness over the
    /// git-detected spaces.
    watch: Arc<git_watch::WatchHub>,
    /// Latest Turn baseline per chat (ADR-0024): in-memory, dropped on
    /// restart. Also carries each Turn's frozen final change set; settled
    /// Turns persist through `turn_change_store`.
    turn_changes: Arc<turn_changes::TurnChanges>,
    /// The skills capability (ADR-0005/0006): root resolution and catalog
    /// assembly over the upstream loader.
    skills: skills::Skills,
    /// Managed images (pasted screenshots) plus the bounded preview-read and
    /// cleanup surface behind `ReadImage`/`StageImage`/`ReleaseImage`.
    images: Arc<images::ImageStore>,
    /// Engine-owned title-task settings (ADR-0012).
    title_settings: title_settings::TitleSettingsStore,
    /// Engine-owned sticky permission-mode default (ADR-0014): the mode new
    /// chats inherit; first launch defaults to confirm-changes.
    mode_default: mode_default::ModeDefaultStore,
    /// Engine-owned web-search settings (ADR-0023): the user-chosen search
    /// backend record behind the `web_search` tool's mounting.
    web_search: web_search_settings::WebSearchStore,
    /// Test-injected backend resolver (`EngineConfig`); production resolves
    /// through the built-in adapter table (which the backend slices fill
    /// in).
    search_backend_resolver: Option<SearchBackendResolver>,
    terminals: Arc<terminals::Terminals>,
    /// The Turn terminal event dispatcher (ADR-0019): fire-and-forget
    /// fan-out of durably settled main-chat Turn outcomes.
    turn_events: turn_events::TurnEvents,
}

impl LocalEngine {
    /// Assemble the local backend against a data dir. Takes the instance lock and
    /// resolves a stable device id.
    pub fn assemble(config: &EngineConfig) -> Result<Self, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let device_id = load_or_create_device_id(&config.data_dir)?;
        let mut spaces = load_spaces(&config.data_dir)?;
        // Lazy checkout-identity backfill (ADR-0002): git-detected spaces
        // persisted before this feature gain their canonical identity now.
        let backfilled = backfill_checkout_ids(&device_id, &mut spaces);
        if backfilled {
            store::persist_spaces(&config.data_dir, &spaces)?;
        }
        let spaces = Arc::new(RwLock::new(spaces));
        let spaces_value = {
            let spaces = spaces
                .read()
                .map_err(|_| EngineError::Other("spaces lock poisoned".into()))?;
            serde_json::to_value(&*spaces).map_err(|error| EngineError::Other(error.to_string()))?
        };
        let (spaces_tx, _) = watch::channel(spaces_value);
        let runtime = Arc::new(AgentRuntime::new(
            device_id.clone(),
            WorkspaceScope::Local,
            config.data_dir.clone(),
            load_chats(&config.data_dir)?,
            config.stream_fn.clone(),
        ));
        let credentials = Arc::new(HoltCredentialStore::load(&config.data_dir)?);
        let provider_settings = Arc::new(ProviderSettingsStore::load(&config.data_dir)?);
        let providers = Arc::new(ProviderAdapter::new(credentials, provider_settings));
        let git = git::Git::new();
        let skills = skills::Skills::new(&config.data_dir, config.personal_skills_dir.as_deref());
        let title_settings = title_settings::TitleSettingsStore::load(&config.data_dir)?;
        let mode_default = mode_default::ModeDefaultStore::load(&config.data_dir)?;
        let web_search = web_search_settings::WebSearchStore::load(&config.data_dir)?;
        let watch = Arc::new(git_watch::WatchHub::new(
            git.clone(),
            device_id.clone(),
            spaces.clone(),
            spaces_tx.subscribe(),
        ));
        Ok(Self {
            service: EngineService {
                engine_info: EngineInfo {
                    device_id,
                    workspace_scope: WorkspaceScope::Local,
                },
                data_dir: config.data_dir.clone(),
                spaces,
                spaces_tx,
                runtime,
                providers,
                git,
                watch,
                turn_changes: Arc::new(turn_changes::TurnChanges::new()),
                skills,
                images: images::assemble(&config.data_dir),
                title_settings,
                mode_default,
                web_search,
                search_backend_resolver: config.search_backend_resolver.clone(),
                terminals: Arc::new(terminals::Terminals::default()),
                turn_events: turn_events::TurnEvents::new(),
            },
            _instance_lock: lock,
        })
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.service.engine_info
    }

    pub fn shutdown(&self) {
        self.service.runtime.shutdown();
        self.service.terminals.close_all(true);
    }
}

impl Drop for LocalEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Stamp git-detected spaces that predate checkout identities with their
/// canonical id (ADR-0002). Returns whether anything changed.
fn backfill_checkout_ids(device_id: &str, spaces: &mut [Space]) -> bool {
    let mut changed = false;
    for space in spaces {
        if space.git_detected && space.checkout_id.is_none() {
            space.checkout_id = git::discover_git_dir(Path::new(&space.path))
                .map(|git_dir| git::checkout_identity(device_id, &git_dir));
            changed |= space.checkout_id.is_some();
        }
    }
    changed
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
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let first = LocalEngine::assemble(&config).unwrap();
        let id = first.engine_info().device_id.clone();
        // The lock dies with the engine; a fresh assemble reads the same id.
        drop(first);
        let second = LocalEngine::assemble(&config).unwrap();
        assert_eq!(second.engine_info().device_id, id);
    }

    #[test]
    fn second_engine_on_one_data_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let _first = LocalEngine::assemble(&config).unwrap();
        assert!(LocalEngine::assemble(&config).is_err());
    }

    #[tokio::test]
    async fn create_space_updates_watch_and_survives_restart() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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

        let engine = LocalEngine::assemble(&config).unwrap();
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
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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
                .service
                .providers
                .resolve_model("openai", custom_id)
                .unwrap()
                .id,
            "gpt-private-2026-09-01"
        );
        drop(engine);

        let restored = LocalEngine::assemble(&config).unwrap();
        assert!(
            restored
                .service
                .providers
                .models_for("openai")
                .iter()
                .any(|model| model.id == custom_id)
        );
    }

    #[tokio::test]
    async fn provider_credential_rpc_keeps_secrets_out_of_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let engine = LocalEngine::assemble(&EngineConfig {
            data_dir: dir.path().into(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
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
        let engine = LocalEngine::assemble(&EngineConfig {
            data_dir: dir.path().into(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
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
    async fn run_against_unconfigured_variant_remains_pending() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().into(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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
        engine
            .handle(methods::QUEUE_COMMAND, run("minimax"))
            .await
            .unwrap();
        let chat = engine.service.runtime.chat("chat-1");
        let mut watch = chat.queue.lock().unwrap().tx.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if watch.borrow()["paused"] == true {
                    break;
                }
                watch.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(
            watch.borrow()["pending"][0]["error"]
                .as_str()
                .unwrap()
                .contains("not configured")
        );

        engine
            .handle(
                methods::SAVE_PROVIDER_KEY,
                serde_json::json!({"providerId": "minimax", "key": "secret-2"}),
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .service
                .providers
                .credentials
                .reveal_key("minimax")
                .await
                .as_deref(),
            Some("secret-2")
        );
        drop(engine);

        // Keys are per-variant and survive restart.
        let restored = LocalEngine::assemble(&config).unwrap();
        assert_eq!(
            restored
                .service
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
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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

        let engine = LocalEngine::assemble(&config).unwrap();
        {
            let chats = engine.service.runtime.chats.read().unwrap();
            let selection = chats[0].config.as_ref().unwrap();
            assert_eq!(selection.provider.as_str(), "openai");
            assert_eq!(selection.model, "openai/gpt-5.4");
        }
        assert!(
            engine
                .service
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
        let engine = LocalEngine::assemble(&EngineConfig {
            data_dir: dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
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
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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
        let engine = LocalEngine::assemble(&config).unwrap();
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
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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
        engine.service.runtime.chats.write().unwrap()[0].last_message_at = Some(chrono::Utc::now());
        engine.service.runtime.publish_chats();
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
        let probe = engine.service.runtime.chats_tx.subscribe();
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
        let engine = LocalEngine::assemble(&config).unwrap();
        let chats = engine.service.runtime.chats.read().unwrap();
        assert!(chats[0].last_seen_at.is_some());
    }

    #[tokio::test]
    async fn delete_chat_removes_persists_and_drops_transcript() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
        };
        let engine = LocalEngine::assemble(&config).unwrap();
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
        let engine = LocalEngine::assemble(&config).unwrap();
        let chats = engine.service.runtime.chats.read().unwrap();
        let ids: Vec<&str> = chats.iter().map(|chat| chat.id.as_str()).collect();
        assert_eq!(ids, ["chat-2"]);
    }
}
