//! The RPC surface: `RpcService` dispatch plus the domain handler
//! modules it routes to. This module owns the dispatch match, the shared
//! param helpers, and the watch stream plumbing; the domain slices of
//! `EngineService` live in the sibling modules.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::watch;

use holt_doc::{SessionMessageEntry, TranscriptFrame, diff_transcript};
use holt_proto::AuthState;
use holt_rpc::{RpcError, RpcReply, RpcService, methods};

use crate::agent::ChatRuntime;
use crate::local_fs::{list_drives, list_folders, local_device};
use crate::{EngineService, LocalEngine};

mod git_surface;
mod images;
mod modes;
mod providers;
pub(crate) mod routines;
mod run;
mod settings;
mod spaces;
mod terminals;
mod workspace;

impl EngineService {
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

    pub(super) fn watch_value(receiver: watch::Receiver<serde_json::Value>) -> RpcReply {
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

    fn fetch_tool_blob(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let blob_ref = required_string(&params, "blobRef")?;
        let (parent, id) = blob_ref
            .split_once('/')
            .ok_or_else(|| RpcError::BadParams("Invalid subagent blob reference".into()))?;
        if crate::subagents::parent_id(id) != Some(parent) {
            return Err(RpcError::BadParams(
                "Invalid subagent blob reference".into(),
            ));
        }
        let child = self
            .runtime
            .subagents
            .load(&self.runtime, id)
            .map_err(RpcError::Failed)?;
        let entries = child.transcript.read().unwrap_or_else(|e| e.into_inner());
        if entries
            .iter()
            .any(|entry| entry.status == Some(holt_doc::MessageStatus::Streaming))
        {
            return Err(RpcError::Failed("Subagent is still running".into()));
        }
        let text = serde_json::to_string(&*entries).map_err(|e| RpcError::Failed(e.to_string()))?;
        RpcReply::value(&serde_json::json!({"text": text}))
    }
}

pub(super) fn required_string<'a>(
    params: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{field} is required")))
}

/// An optional string param: blank counts as absent.
pub(super) fn optional_string(params: &serde_json::Value, field: &str) -> Option<String> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

pub(super) fn required_string_list(
    params: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, RpcError> {
    params
        .get(field)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .ok_or_else(|| RpcError::BadParams(format!("{field} must be a list of strings")))
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

/// A stream that never emits — for subscriptions this backend has no data for.
fn pending_stream() -> RpcReply {
    use futures::StreamExt;
    RpcReply::Stream(futures::stream::pending::<serde_json::Value>().boxed())
}

#[async_trait]
impl RpcService for LocalEngine {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.service.handle(method, params).await
    }
}

#[async_trait]
impl RpcService for EngineService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::OPEN_TERMINAL => self.open_terminal(params).await,
            methods::SUBSCRIBE_TERMINAL => self.subscribe_terminal(params),
            methods::WRITE_TERMINAL => self.write_terminal(params).await,
            methods::RESIZE_TERMINAL => self.resize_terminal(params),
            methods::LIST_TERMINALS => RpcReply::value(&self.terminals.list()),
            methods::CLOSE_TERMINAL | methods::CLOSE_ALL_TERMINALS => {
                self.close_terminal(method, params).await
            }
            methods::WATCH_MESSAGE_QUEUE => self.watch_message_queue(params),
            methods::WATCH_CHAT_USAGE => self.watch_chat_usage(params),
            methods::USAGE_STATS => self.usage_stats(params).await,
            methods::WATCH_TURN_TERMINAL_EVENTS => self.watch_turn_terminal_events(),
            methods::WATCH_TURN_RETRY => self.watch_turn_retry(),
            methods::CONTINUE_MESSAGE_QUEUE => self.continue_message_queue(params),
            methods::EDIT_QUEUED_MESSAGE => self.edit_queued_message(params),
            methods::EDIT_LAST_MESSAGE => self.edit_last_message(params).await,
            methods::DELETE_QUEUED_MESSAGE => self.delete_queued_message(params),
            methods::UPDATE_STATUS => Ok(Self::watch_value(self.updater.subscribe())),
            methods::APPLY_UPDATE => {
                self.updater.apply().await.map_err(RpcError::Failed)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::CANCEL_UPDATE => {
                self.updater.cancel();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => RpcReply::value(&serde_json::json!({ "ready": true })),
            methods::LOCAL_DEVICE => RpcReply::value(&serde_json::json!({
                "deviceId": self.engine_info.device_id,
            })),
            methods::LIST_PROVIDERS => RpcReply::value(&self.providers.providers().await),
            methods::SAVE_PROVIDER_KEY => self.save_provider_key(params).await,
            methods::PROBE_PROVIDER => self.probe_provider(params).await,
            methods::REVEAL_PROVIDER_KEY => self.reveal_provider_key(params).await,
            methods::REMOVE_PROVIDER_KEY => self.remove_provider_key(params).await,
            methods::LIST_MODELS => self.list_models(params),
            methods::LIST_HIDDEN_MODELS => self.list_hidden_models(params),
            methods::APPLY_MODEL_PROPOSAL => self.apply_model_proposal(params),
            methods::DISCARD_MODEL_PROPOSAL => self.discard_model_proposal(params),
            methods::SETTLE_PROVIDER_KEY_REQUEST => self.settle_provider_key_request(params).await,
            methods::SETTLE_PROVIDER_CHOICE => self.settle_provider_choice(params).await,
            methods::SETTLE_QUESTION => self.settle_question(params).await,
            methods::DISMISS_QUESTION => self.dismiss_question(params),
            methods::LIST_API_DIALECTS => self.list_api_dialects(),
            methods::SAVE_CUSTOM_PROVIDER => self.save_custom_provider(params),
            methods::REMOVE_CUSTOM_PROVIDER => self.remove_custom_provider(params),
            methods::SET_PROVIDER_LOGO => self.set_provider_logo(params),
            methods::REMOVE_PROVIDER_LOGO => self.remove_provider_logo(params),
            methods::SAVE_MODEL_RECORD => self.save_model_record(params),
            methods::REMOVE_MODEL_RECORD => self.remove_model_record(params),
            methods::SET_HIDDEN_MODELS => self.set_hidden_models(params),
            methods::RESET_PROVIDER_CATALOG => self.reset_provider_catalog(params),
            methods::GET_TITLE_SETTINGS => RpcReply::value(&self.title_settings_state().await),
            methods::SAVE_TITLE_SETTINGS => self.save_title_settings(params).await,
            methods::GET_GOAL_SETTINGS => RpcReply::value(&self.goal_settings_state().await),
            methods::SAVE_GOAL_SETTINGS => self.save_goal_settings(params).await,
            methods::GET_WEB_SEARCH_SETTINGS => RpcReply::value(&self.web_search_state()),
            methods::SAVE_WEB_SEARCH_BACKEND => self.save_web_search_backend(params),
            methods::SET_ACTIVE_WEB_SEARCH_BACKEND => self.set_active_web_search_backend(params),
            methods::REVEAL_WEB_SEARCH_KEY => self.reveal_web_search_key(params),
            methods::REMOVE_WEB_SEARCH_BACKEND => self.remove_web_search_backend(params),
            // MCP servers (ADR-0034): the Settings quartet — get with
            // validation feedback, strict upsert, remove, and the
            // on-demand probe. No standing watch: probing on demand is
            // what keeps startup lazy.
            methods::GET_MCP_SETTINGS => RpcReply::value(&self.mcp_settings_state().await),
            methods::SAVE_MCP_SERVER => self.save_mcp_server(params).await,
            methods::REMOVE_MCP_SERVER => self.remove_mcp_server(params).await,
            methods::TEST_MCP_SERVER => {
                let name = required_string(&params, "name")?;
                RpcReply::value(&self.mcp_probe_reply(name).await)
            }
            // The composer's slash menu (ADR-0011/0025/0037): the commands this
            // backend intercepts itself.
            methods::LIST_COMMANDS => RpcReply::value(&serde_json::json!([
                { "name": "compact", "description": "Summarize the older conversation and keep only a recent tail" },
                { "name": "init", "description": "Generate or update AGENTS.md for this repository" },
                { "name": "plan", "description": "Plan Mode: explore read-only, submit a plan for approval", "inputHint": "[task]" },
                { "name": "provider", "description": "Provider Mode: add or update providers and models by conversation", "inputHint": "[task | off]" },
                { "name": "goal", "description": "Goal Mode: loop on an objective until a verifier says it holds", "inputHint": "[objective | off | pause | resume]" }
            ])),
            // The skills catalog (ADR-0005): fresh per call — the
            // filesystem is the registry, so there is nothing to cache.
            // Absent roots are skipped silently inside the scan.
            methods::LIST_SKILLS => {
                let cwd = params
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let listing = self.skills.catalog(cwd.as_deref()).await.listing();
                RpcReply::value(&listing)
            }
            // Local folder browsing for the add-space palette. The UI only
            // targets remote devices over the relay; here every browse is local.
            methods::LIST_FOLDERS => match list_folders(&params) {
                Ok(listing) => RpcReply::value(&listing),
                Err(message) => Err(RpcError::Failed(message)),
            },
            methods::LIST_DRIVES => RpcReply::value(&list_drives()),

            methods::SEARCH_FILES => self.search_files(params).await,

            methods::LIST_WORKSPACE_ENTRIES => self.list_workspace_entries(params).await,
            methods::READ_WORKSPACE_FILE => self.read_workspace_file(params).await,
            methods::READ_WORKSPACE_IMAGE => self.read_workspace_image(params).await,
            methods::SAVE_WORKSPACE_FILE => self.save_workspace_file(params).await,
            methods::WRITE_WORKSPACE_FILE_AS => self.write_workspace_file_as(params).await,
            methods::WATCH_WORKSPACE_ENTRIES => self.watch_workspace_entries(params),
            methods::WATCH_WORKSPACE_GIT_STATUS => self.watch_workspace_git_status(params),
            methods::CREATE_WORKSPACE_ENTRY => self.create_workspace_entry(params).await,
            methods::RENAME_WORKSPACE_ENTRY => self.rename_workspace_entry(params).await,
            methods::MOVE_WORKSPACE_ENTRY => self.move_workspace_entry(params).await,
            methods::TRASH_WORKSPACE_ENTRY => self.trash_workspace_entry(params).await,

            methods::LIST_REFS => self.list_refs(params).await,
            methods::LIST_BRANCHES => self.list_branches(params).await,
            methods::SWITCH_REF => self.switch_ref(params).await,
            methods::CREATE_BRANCH => self.create_branch(params).await,

            // Entity watches: one snapshot, then silence. Devices are real —
            // the local machine browses its own folders — the rest stay empty.
            methods::WATCH_DEVICES => {
                let value = serde_json::to_value(vec![local_device(&self.engine_info.device_id)])
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(static_watch(value))
            }
            methods::WATCH_CHATS => Ok(Self::watch_value(self.runtime.chats_tx.subscribe())),
            methods::WATCH_SESSIONS => Ok(Self::watch_value(self.runtime.sessions_tx.subscribe())),
            methods::WATCH_SPACES => Ok(self.watch_spaces()),
            methods::LIST_ROUTINES => RpcReply::value(&self.routines.views()),
            methods::WATCH_ROUTINES => Ok(Self::watch_value(self.routines.subscribe())),
            methods::CREATE_ROUTINE => self.create_routine(params),
            methods::UPDATE_ROUTINE => self.update_routine(params),
            methods::PREVIEW_ROUTINE_SCHEDULE => self.preview_routine_schedule(params),
            methods::DELETE_ROUTINE => self.delete_routine(params),
            methods::SET_ROUTINE_PAUSED => self.set_routine_paused(params),
            methods::RUN_ROUTINE_NOW => self.run_routine_now(params).await,
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
                let chat = if chat_id.contains("--sub--") {
                    self.runtime
                        .subagents
                        .load(&self.runtime, chat_id)
                        .map_err(RpcError::Failed)?
                } else {
                    self.runtime.chat(chat_id)
                };
                Ok(Self::watch_transcript(chat))
            }
            methods::FETCH_TOOL_BLOB => self.fetch_tool_blob(params),
            methods::WATCH_CHECKOUT_CHANGE_REQUEST => Ok(pending_stream()),

            // Live checkout-diff awareness (git-capability issue 03): the
            // hub owns one watcher per git space; first subscriber starts
            // it, last stops it.
            methods::WATCH_CHECKOUT_DIFFS => Ok(self.watch.subscribe()),

            methods::GET_CHECKOUT_DIFF => self.get_checkout_diff(params).await,
            methods::GET_CHECKOUT_FILE_DIFF_TEXT => self.get_checkout_file_diff_text(params).await,

            methods::GET_TURN_CHANGE_SET => self.get_turn_change_set(params).await,
            methods::WATCH_TURN_CHANGE_SET => self.watch_turn_change_set(params).await,
            methods::RESTORE_TURN_CHANGES => self.restore_turn_changes(params).await,

            methods::LIST_GIT_HISTORY => self.list_git_history(params).await,
            methods::FETCH_ALL => self.fetch_all(params).await,
            // No-op liveness pokes the UI fires defensively.
            methods::PROBE_SYNC => RpcReply::value(&serde_json::json!({})),

            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("createSpace") =>
            {
                self.create_space(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("renameSpace") =>
            {
                self.rename_space(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("deleteSpace") =>
            {
                self.delete_space(params)
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
                if params.get("op").and_then(|op| op.as_str()) == Some("setChatPermissionMode") =>
            {
                self.set_chat_permission_mode(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("setChatArchived") =>
            {
                self.set_chat_archived(params)
            }
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("setChatPinned") =>
            {
                self.set_chat_pinned(params)
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
            methods::MUTATE
                if params.get("op").and_then(|op| op.as_str()) == Some("renameChat") =>
            {
                self.rename_chat(params)
            }
            methods::QUEUE_COMMAND => self.queue_command(params).await,
            // Confirm-changes verdicts (ADR-0014): params
            // `{approvalId, verdict}` with `holt_proto::ApprovalVerdict`
            // as the verdict.
            methods::RESOLVE_APPROVAL => self.resolve_approval(params),
            // The sticky default new chats inherit (ADR-0014) — the
            // new-chat canvas chip reads it so it can advertise the mode a
            // first send would actually run under.
            methods::GET_PERMISSION_MODE_DEFAULT => {
                RpcReply::value(&serde_json::json!({ "mode": self.mode_default.get() }))
            }
            methods::ENTER_PLAN_MODE => self.enter_plan_mode(params),
            methods::EXIT_PLAN_MODE => self.exit_plan_mode(params),
            methods::SET_GOAL => self.set_goal(params),
            methods::CLEAR_GOAL => self.clear_goal(params),
            methods::SET_GOAL_PAUSED => self.set_goal_paused(params),
            methods::ENTER_PROVIDER_MODE => self.enter_provider_mode(params),
            methods::EXIT_PROVIDER_MODE => self.exit_provider_mode(params),
            methods::GET_PROVIDER_MODE => {
                RpcReply::value(&self.provider_mode_state(required_string(&params, "chatId")?)?)
            }
            methods::GET_PLAN_MODE => {
                RpcReply::value(&self.plan_mode_state(required_string(&params, "chatId")?)?)
            }
            methods::RESOLVE_PLAN_APPROVAL => self.resolve_plan_approval(params),

            methods::READ_IMAGE => self.read_image(params).await,
            methods::STAGE_IMAGE => self.stage_image(params).await,
            methods::RELEASE_IMAGE => self.release_image(params).await,

            // Everything this backend has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
