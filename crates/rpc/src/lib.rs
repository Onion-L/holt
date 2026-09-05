//! holt-rpc — the typed control plane (UiRpc / ControlRpc) over in-memory
//! string transports.
//!
//! Framing: ndjson envelopes, one JSON object per line, matching the shape of
//! holt's Effect RPC without the Effect runtime:
//!
//! - client → server: `{id, method, params}` to invoke, `{id, cancel: true}` to stop a stream;
//! - server → client: `{id, ok}` / `{id, err}` for unary calls,
//!   `{id, item}`* then `{id, done: true}` (or `{id, err}`) for streams.
//!
//! The server dispatches into an [`RpcService`]; the [`RpcClient`] offers `call` and
//! `subscribe`. Both ends run over any pair of string channels, so the in-memory
//! transport ([`memory_client`]) is the only transport: the UI embeds its engine
//! in-process.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
mod server;

pub use client::{RpcClient, RpcSubscription};
pub use server::serve_connection;

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/research/feature-inventory.md §2.
pub mod methods {
    pub const LIST_PROVIDERS: &str = "ListProviders";
    pub const SAVE_PROVIDER_KEY: &str = "SaveProviderKey";
    pub const REVEAL_PROVIDER_KEY: &str = "RevealProviderKey";
    pub const REMOVE_PROVIDER_KEY: &str = "RemoveProviderKey";
    pub const ADD_PROVIDER_MODEL: &str = "AddProviderModel";
    /// Drops one user-added model: params `{providerId, modelId}`. Builtin
    /// catalog ids are not custom models — the engine no-ops on them.
    pub const REMOVE_PROVIDER_MODEL: &str = "RemoveProviderModel";
    pub const LIST_MODELS: &str = "ListModels";
    /// Engine-owned title-task settings (ADR-0012). Read takes no params;
    /// save params are `holt_proto::TitleSettings`; both reply with
    /// `holt_proto::TitleSettingsState` (settings + live validation warning).
    /// An empty model id disables automatic titles.
    pub const GET_TITLE_SETTINGS: &str = "GetTitleSettings";
    pub const SAVE_TITLE_SETTINGS: &str = "SaveTitleSettings";
    pub const LIST_COMMANDS: &str = "ListCommands";
    /// The skills catalog (ADR-0005): one fresh scan of the chat's three
    /// skill roots. Params `{cwd?}` — the project root derives from it;
    /// reply `SkillListing` (invocable entries with source root, shadowed
    /// entries, load diagnostics).
    pub const LIST_SKILLS: &str = "ListSkills";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    /// Per-chat `holt_proto::MessageQueue` snapshots. Params `{chatId}`.
    pub const WATCH_MESSAGE_QUEUE: &str = "WatchMessageQueue";
    /// Resume automatic queue execution after Stop, failure, or restart.
    /// Params `{chatId}`; replies with the accepted `MessageQueue` snapshot.
    pub const CONTINUE_MESSAGE_QUEUE: &str = "ContinueMessageQueue";
    /// Edit a pending ordinary message's body: params `{chatId, messageId,
    /// prompt}`; replies with the accepted `MessageQueue` snapshot. Identity,
    /// position, command kind, and the captured model/reasoning are the
    /// queue's — an item that already started fails without mutating it.
    pub const EDIT_QUEUED_MESSAGE: &str = "EditQueuedMessage";
    /// Remove a pending ordinary message from the queue: params
    /// `{chatId, messageId}`; replies with the accepted `MessageQueue`
    /// snapshot. Remaining items keep their order; a started item fails
    /// instead of touching the active Turn.
    pub const DELETE_QUEUED_MESSAGE: &str = "DeleteQueuedMessage";
    /// Resolve a pending confirm-changes Approval (ADR-0014): params
    /// `{approvalId, verdict}` where verdict is
    /// `holt_proto::ApprovalVerdict` (`{"kind":"allow"}`,
    /// `{"kind":"alwaysAllow"}`, or `{"kind":"deny","note?":"…"}`). The
    /// verdict releases the gate the run is blocked in; unknown ids fail —
    /// an approval resolves once.
    pub const RESOLVE_APPROVAL: &str = "ResolveApproval";
    /// User-driven delivery retry for a chat with unadopted queued sends:
    /// fresh chat2 socket, host nudge, drain pass, and a new delivery escort
    /// per pending command. Params `{chatId}`.
    pub const RETRY_DELIVERY: &str = "RetryDelivery";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    /// Nudge every open subscription to verify liveness NOW (window focus,
    /// app foregrounded). No params; cheap to spam.
    pub const PROBE_SYNC: &str = "ProbeSync";
    /// Pushed edge-connectivity posture (`holt_proto::Connectivity`):
    /// current value first, then every change — the connection pill /
    /// composer-honesty / queued-badge feed. No params.
    pub const WATCH_CONNECTIVITY: &str = "WatchConnectivity";
    /// In-flight queued-attachment transfers (`holt_proto::TransferProgress`
    /// list): current set first, then a fresh snapshot per landed chunk —
    /// the sending thumbnail's percent-ring feed. No params.
    pub const WATCH_TRANSFERS: &str = "WatchTransfers";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    /// Spaces registry (device+folder pairs) from the workspace doc.
    pub const WATCH_SPACES: &str = "WatchSpaces";
    /// Entity mutations against the workspace doc (feature-inventory §2 DataRpc).
    /// Params are tagged `{op: createChat|createSpace|renameSpace|deleteSpace|
    /// renameChat|setChatArchived|deleteChat|renameDevice|markChatSeen|
    /// setChatConfig|setChatPermissionMode, …}`. `setChatPermissionMode`
    /// takes `{chatId, mode}` with the ADR-0014 kebab-case tiers.
    pub const MUTATE: &str = "Mutate";
    /// This engine's identity → `{deviceId}`.
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    /// This engine runtime's fixed device and workspace identity.
    pub const ENGINE_INFO: &str = "EngineInfo";
    /// Readiness barrier for the engine runtime. The call completes once stores
    /// and journals are assembled, or fails with the assembly error.
    pub const ENGINE_READY: &str = "EngineReady";
    /// Ask a headless IPC owner to drain its runtime and exit successfully.
    /// Headed IPC owners do not implement this method: closing another app's
    /// engine behind its windows would leave that process unusable.
    pub const STOP_ENGINE: &str = "StopEngine";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // AuthRpc mutations.
    pub const SIGN_IN: &str = "SignIn";
    pub const SIGN_IN_HEADLESS: &str = "SignInHeadless";
    pub const COMPLETE_SIGN_IN: &str = "CompleteSignIn";
    pub const SIGN_OUT: &str = "SignOut";
    pub const LIST_ORGS: &str = "ListOrgs";
    pub const CREATE_ORG: &str = "CreateOrg";
    pub const SELECT_ORG: &str = "SelectOrg";
    /// One-time local→synced profile import: what's importable (unary).
    pub const LOCAL_IMPORT_STATUS: &str = "LocalImportStatus";
    /// One-time local→synced profile import: run it (stream of progress items).
    pub const IMPORT_LOCAL_WORKSPACE: &str = "ImportLocalWorkspace";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    pub const LIST_GIT_HISTORY: &str = "ListGitHistory";
    /// Update remote-tracking refs without changing HEAD, the index, or files.
    pub const FETCH_ALL: &str = "FetchAll";
    pub const SWITCH_REF: &str = "SwitchRef";
    /// Create a branch and check it out (`checkout -b` semantics): params
    /// `{repoPath, name, baseRef?}`. Base defaults to the current HEAD; the
    /// optional base ref is contract headroom (the v1 UI input is name-only).
    /// Invalid or already-existing names are rejected with git's message;
    /// the checkout obeys the same safe-checkout rules as `SwitchRef`.
    pub const CREATE_BRANCH: &str = "CreateBranch";
    pub const LIST_FOLDERS: &str = "ListFolders";
    /// The device's browse roots: home plus mounted drives/volumes.
    pub const LIST_DRIVES: &str = "ListDrives";
    /// Fuzzy relative-path search rooted in a known chat or space checkout.
    pub const SEARCH_FILES: &str = "SearchFiles";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    // Terminals (ControlRpc, relay-forwardable; SubscribeTerminal streams).
    pub const OPEN_TERMINAL: &str = "OpenTerminal";
    pub const SUBSCRIBE_TERMINAL: &str = "SubscribeTerminal";
    pub const WRITE_TERMINAL: &str = "WriteTerminal";
    pub const RESIZE_TERMINAL: &str = "ResizeTerminal";
    pub const CLOSE_TERMINAL: &str = "CloseTerminal";
    /// Checkout-diff stream for the target device's chats (DataRpc,
    /// relay-forwardable — diffs are produced where the checkout lives).
    pub const WATCH_CHECKOUT_DIFFS: &str = "WatchCheckoutDiffs";
    /// Current pull request for one checkout, resolved on the checkout's host device.
    pub const WATCH_CHECKOUT_CHANGE_REQUEST: &str = "WatchCheckoutChangeRequest";
    pub const GET_CHECKOUT_DIFF: &str = "GetCheckoutDiff";
    pub const GET_CHECKOUT_FILE_DIFF_TEXT: &str = "GetCheckoutFileDiffText";
    // Agent accounts (ControlRpc, relay-forwardable — CLI logins are per-device).
    // Uploads / attachments (ControlRpc, relay-forwardable — target the chat's host device).
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    /// Lazy full-tool-output fetch from the R2 sidecar by doc-resident ref
    /// (chat2-sync A3). Edge-direct from any device — never relay-forwarded.
    pub const FETCH_TOOL_BLOB: &str = "FetchToolBlob";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("bad params: {0}")]
    BadParams(String),
    #[error("{0}")]
    Failed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
}

/// A client-originated frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

/// A server-originated frame. Exactly one of `ok` / `err` / `item` / `done` is meaningful.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
}

impl RpcReply {
    /// Serialize a value into a unary reply.
    pub fn value<T: Serialize>(value: &T) -> Result<Self, RpcError> {
        serde_json::to_value(value)
            .map(RpcReply::Value)
            .map_err(|e| RpcError::Failed(format!("serialize response: {e}")))
    }
}

/// Server-side dispatch: one implementation serves every transport.
#[async_trait]
pub trait RpcService: Send + Sync + 'static {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError>;
}

/// Deserialize typed params out of the envelope's `params` value.
pub fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::BadParams(e.to_string()))
}

/// Spawn an in-memory server for `service` and return a connected client.
/// Same envelopes, same dispatch loop as the WebSocket path — the in-process UI
/// transport (ARCHITECTURE §1 "zero serialization shortcuts").
pub fn memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<String>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(serve_connection(service, server_out, server_in));
    RpcClient::new(client_out, client_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Mutex;

    struct TestService;

    struct CancelAwareService {
        dropped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(dropped) = self.0.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[async_trait]
    impl RpcService for CancelAwareService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if method != methods::WATCH_CHECKOUT_CHANGE_REQUEST {
                return Err(RpcError::UnknownMethod(method.into()));
            }
            let guard = DropSignal(self.dropped.lock().unwrap().take());
            let stream = futures::stream::unfold(guard, |guard| async move {
                let item = std::future::pending::<Option<(serde_json::Value, DropSignal)>>().await;
                drop(guard);
                item
            });
            Ok(RpcReply::Stream(stream.boxed()))
        }
    }

    #[async_trait]
    impl RpcService for TestService {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "Count" => {
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    Ok(RpcReply::Stream(
                        futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                    ))
                }
                "Never" => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
                "Boom" => Err(RpcError::Failed("boom".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 3}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(v) = items.recv().await {
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2)
            ]
        );

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(m) if m == "boom"));
    }

    #[tokio::test]
    async fn checked_stream_acknowledges_support_and_preserves_unknown_method() {
        let client = memory_client(Arc::new(TestService));

        let mut items = client
            .subscribe_checked("Count", serde_json::json!({"n": 1}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, None);

        let error = match client
            .subscribe_checked("FutureStream", serde_json::Value::Null)
            .await
        {
            Ok(_) => panic!("old service must reject unknown stream"),
            Err(error) => error,
        };
        assert!(matches!(error, RpcError::UnknownMethod(method) if method == "FutureStream"));
    }

    #[tokio::test]
    async fn dropping_checked_subscription_cancels_pending_server_stream() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let client = memory_client(Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(dropped_tx)),
        }));
        let stream = client
            .subscribe_checked(
                methods::WATCH_CHECKOUT_CHANGE_REQUEST,
                serde_json::Value::Null,
            )
            .await
            .unwrap();

        drop(stream);

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("server stream cancelled")
            .expect("drop signal");
    }

    #[tokio::test]
    async fn dropping_stream_receiver_cancels_server_side() {
        let client = memory_client(Arc::new(TestService));
        let items = client
            .subscribe("Never", serde_json::Value::Null)
            .await
            .unwrap();
        drop(items);
        // The next unary call still works — the dead stream didn't wedge the connection.
        let echoed = client.call("Echo", serde_json::json!(2)).await.unwrap();
        assert_eq!(echoed, serde_json::json!(2));
    }
}
