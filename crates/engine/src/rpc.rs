//! The RPC surface: `RpcService` dispatch plus the space/chat mutation and
//! queue-command handlers it routes to.

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry, TranscriptFrame,
    diff_transcript,
};
use holt_proto::{
    AuthState, Chat, ChatConfig, JevSettingsState, PendingKind, ProviderId, ReasoningLevel,
    RunRequest, SessionStatus, Space, TitleSettings, TitleSettingsState, TitleSource,
    TurnChangeSetReply, WebSearchBackendOption, WebSearchSettingsState,
};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use pi_core::ai::types::Model as CoreModel;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentRun, ChatRuntime};
use crate::local_fs::{list_drives, list_folders, local_device};
use crate::store::{persist_chats, persist_spaces};
use crate::title_settings::MAX_TITLE_INSTRUCTION_CHARS;
use crate::{EngineService, LocalEngine};

/// The explicit non-Git answer `GetTurnChangeSet`/`WatchTurnChangeSet`
/// share (ADR-0024): an empty change set must never stand in for it.
const NON_GIT_CHANGE_SET_REASON: &str = "the chat's working directory is not a Git work tree";

/// The turn-diff scopes' soft-matchable phrase for a chat whose current Turn
/// has no recorded baseline (never ran, engine restarted).
const NO_TURN_RECORDED: &str = "no turn recorded for this chat yet";

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
        // Mint the canonical checkout identity at create time (ADR-0002):
        // sha256(deviceId ‖ NUL ‖ git_dir) for folders that are git work
        // trees. A git_detected path that discovers no repo stays untagged.
        let space = Space {
            checkout_id: params
                .git_detected
                .then(|| {
                    crate::git::discover_git_dir(std::path::Path::new(&params.path))
                        .map(|git_dir| crate::git::checkout_identity(&params.device_id, &git_dir))
                })
                .flatten(),
            id: params.space_id,
            device_id: params.device_id,
            path: params.path,
            name: None,
            git_detected: params.git_detected,
            git_checked_at: None,
            created_at: Utc::now(),
        };
        spaces.push(space);
        persist_spaces(&self.data_dir, &spaces).map_err(|error| {
            spaces.pop();
            RpcError::Failed(error.to_string())
        })?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        self.spaces_tx.send_replace(value);
        RpcReply::value(&serde_json::json!({}))
    }

    /// Remove a space (UI "Remove project"): the row goes, and every chat
    /// in the space goes with it — the confirm dialog promises the sessions
    /// are permanently deleted, matching the workspace-doc cascade. Unknown
    /// ids: idempotent no-op, and no watch frame when nothing moved.
    fn delete_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: DeleteSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.space_id.trim().is_empty() {
            return Err(RpcError::BadParams("spaceId must not be empty".into()));
        }
        let chat_ids: Vec<String> = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = chats
                .iter()
                .filter(|chat| chat.space_id.as_deref() == Some(params.space_id.as_str()))
                .map(|chat| chat.id.clone())
                .collect();
            chats.retain(|chat| chat.space_id.as_deref() != Some(params.space_id.as_str()));
            drop(chats);
            if !ids.is_empty() {
                self.runtime
                    .persist_chats_locked()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
            ids
        };
        for chat_id in &chat_ids {
            self.runtime.remove_chat(chat_id);
            self.terminals.close_chat(chat_id);
        }
        if !chat_ids.is_empty() {
            self.runtime.publish_chats();
        }
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        let before = spaces.len();
        spaces.retain(|space| space.id != params.space_id);
        let removed = spaces.len() != before;
        if removed {
            persist_spaces(&self.data_dir, &spaces)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(spaces);
        if removed {
            self.spaces_tx.send_replace(value);
        }
        RpcReply::value(&serde_json::json!({}))
    }

    /// Rename a space (UI "Rename…"): sets the user-visible name override;
    /// unknown ids are an idempotent no-op, matching the chat paths.
    fn rename_space(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: RenameSpaceParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let name = params.name.trim();
        if params.space_id.trim().is_empty() || name.is_empty() {
            return Err(RpcError::BadParams(
                "spaceId and name must not be empty".into(),
            ));
        }
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        let Some(row) = spaces.iter_mut().find(|space| space.id == params.space_id) else {
            return RpcReply::value(&serde_json::json!({}));
        };
        row.name = Some(name.to_string());
        persist_spaces(&self.data_dir, &spaces)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(spaces);
        self.spaces_tx.send_replace(value);
        RpcReply::value(&serde_json::json!({}))
    }

    /// Starts a fresh hidden `model-setup` chat (model setup v2). The
    /// dialog session IS the chat's whole lifetime — the UI deletes it on
    /// close — so nothing carries across opens: no transcript memory for
    /// the model, no stale proposals. Any earlier setup chat still on
    /// record (a dialog killed mid-session, a crashed run) is deleted here
    /// outright. The row is archived so the sidebar never lists it.
    fn start_model_setup_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let provider = ProviderId(required_string(&params, "provider")?.to_string());
        let model = required_string(&params, "model")?.to_string();
        let reasoning: Option<ReasoningLevel> = params
            .get("reasoning")
            .filter(|value| !value.is_null())
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let config = ChatConfig {
            provider,
            model,
            reasoning,
            model_options: Default::default(),
            permission_mode: self.mode_default.get(),
            scope: holt_proto::ChatScope::ModelSetup,
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        let stale = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let stale: Vec<String> = chats
                .iter()
                .filter(|chat| {
                    chat.config
                        .as_ref()
                        .is_some_and(|config| config.scope == holt_proto::ChatScope::ModelSetup)
                })
                .map(|chat| chat.id.clone())
                .collect();
            chats.retain(|chat| !stale.contains(&chat.id));
            chats.push(Chat {
                id: chat_id.clone(),
                device_id: self.engine_info.device_id.clone(),
                title: Some("Provider setup".into()),
                title_source: TitleSource::UserManual,
                title_task_started: true,
                archived: true,
                pinned: false,
                cwd: None,
                branch: None,
                checkout_id: None,
                source_context: None,
                config: Some(config),
                last_message_preview: None,
                last_message_at: None,
                created_at: Utc::now(),
                space_id: None,
                last_seen_at: None,
                room_gen: None,
                compact_before_next_turn: false,
                plan_mode: None,
                provider_mode: false,
            });
            persist_chats(&self.data_dir, &chats)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
            stale
        };
        for stale_id in &stale {
            self.runtime.remove_chat(stale_id);
            self.terminals.close_chat(stale_id);
        }
        self.runtime.chat(&chat_id);
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({ "chatId": chat_id }))
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
        // New chats inherit the last mode used on the device (ADR-0014):
        // the sticky default overrides whatever mode the creating client
        // sent — inheritance is engine-owned, read here at creation.
        let config = params.config.map(|mut config| {
            config.permission_mode = self.mode_default.get();
            config
        });
        chats.push(Chat {
            id: params.chat_id.clone(),
            device_id: params
                .device_id
                .or_else(|| space.as_ref().map(|space| space.device_id.clone()))
                .unwrap_or_else(|| self.engine_info.device_id.clone()),
            title: None,
            title_source: TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: params
                .cwd
                .or_else(|| space.as_ref().map(|space| space.path.clone())),
            branch: params.branch,
            checkout_id: space.as_ref().and_then(|space| space.checkout_id.clone()),
            source_context: None,
            config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            space_id: params.space_id,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            provider_mode: false,
        });
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.chat(&params.chat_id);
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    /// The working directory whose Git state a Turn change set reads: the
    /// chat's stamped cwd, else its space's path.
    fn turn_change_root(&self, chat_id: &str) -> Result<String, RpcError> {
        self.search_files_root(&SearchFilesParams {
            query: String::new(),
            chat_id: Some(chat_id.to_string()),
            space_id: None,
        })
    }

    /// The directory `SearchFiles` walks: the chat's own cwd when set,
    /// otherwise its space's path; a space id resolves to the space path
    /// directly. Unknown ids are backend faults, not param errors.
    fn search_files_root(&self, params: &SearchFilesParams) -> Result<String, RpcError> {
        let root = if let Some(chat_id) = params.chat_id.as_deref() {
            let chats = self
                .runtime
                .chats
                .read()
                .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
            let chat = chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::Failed(format!("unknown chat {chat_id}")))?;
            match chat.cwd.clone() {
                Some(cwd) => cwd,
                None => {
                    let space_id = chat.space_id.clone().ok_or_else(|| {
                        RpcError::Failed(format!("chat {chat_id} has no working directory"))
                    })?;
                    let spaces = self
                        .spaces
                        .read()
                        .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
                    spaces
                        .iter()
                        .find(|space| space.id == space_id)
                        .map(|space| space.path.clone())
                        .ok_or_else(|| RpcError::Failed(format!("unknown space {space_id}")))?
                }
            }
        } else {
            let space_id = params.space_id.as_deref().expect("selector checked");
            let spaces = self
                .spaces
                .read()
                .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
            spaces
                .iter()
                .find(|space| space.id == space_id)
                .map(|space| space.path.clone())
                .ok_or_else(|| RpcError::Failed(format!("unknown space {space_id}")))?
        };
        Ok(crate::local_fs::expand_tilde(&root))
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
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    fn set_chat_pinned(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: SetChatPinnedParams = serde_json::from_value(params)
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
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.pinned = params.pinned;
        drop(chats);
        self.runtime
            .persist_chats_locked()
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
            self.runtime
                .persist_chats_locked()
                .map_err(|error| RpcError::Failed(error.to_string()))?;
            self.runtime.remove_chat(&params.chat_id);
            self.terminals.close_chat(&params.chat_id);
            self.runtime.publish_chats();
            // Reclaim at restart, when no in-memory draft or retry owns files.
        }
        // Unknown chat: idempotent no-op, matching the archive path.
        RpcReply::value(&serde_json::json!({}))
    }

    fn rename_chat(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: RenameChatParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if params.chat_id.trim().is_empty() {
            return Err(RpcError::BadParams("chatId must not be empty".into()));
        }
        let title = params.title.trim();
        if title.is_empty() {
            return Err(RpcError::BadParams("title must not be empty".into()));
        }
        let title = title.to_string();
        let mut chats = self
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == params.chat_id) else {
            // Unknown chat: idempotent no-op, matching the archive path.
            return RpcReply::value(&serde_json::json!({}));
        };
        row.title = Some(title);
        // A manual rename locks the title even when the text is unchanged,
        // so ownership is unambiguous — always persist and publish.
        row.title_source = TitleSource::UserManual;
        drop(chats);
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
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
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        RpcReply::value(&serde_json::json!({}))
    }

    /// A queued run built from the chat's stored config with a prompt
    /// swapped in — the one shape every engine-side enqueue uses (the
    /// composer's sends arrive pre-built; the plan follow-up and the Key
    /// request's settle notices build here).
    fn queued_run_request(
        config: &holt_proto::ChatConfig,
        prompt: &str,
        cwd: String,
    ) -> RunRequest {
        RunRequest {
            prompt: prompt.to_string(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            reasoning: config.reasoning,
            model_options: config.model_options.clone(),
            cwd,
            permission_mode: config.permission_mode,
            auto_approve: false,
            attachments: Vec::new(),
            worktree: None,
        }
    }

    /// An attended send (ADR-0021): a first acceptance arriving while the
    /// chat's execution channel is settled and the queue is paused. Prep
    /// that has not reached the admission checkpoint still occupies the
    /// channel through the driver, so `driver_running` must be quiet too.
    fn attended_send(chat: &ChatRuntime, queue: &super::queue::Queue) -> bool {
        queue.paused() && queue.idle() && !chat.driver_running.load(Ordering::Acquire)
    }

    /// The one engine-side enqueue path for a run command: the composer's
    /// `Run` and the Key request's settle notice both queue through here,
    /// so attended-send and admission semantics cannot drift apart.
    fn enqueue_run(
        &self,
        chat: Arc<ChatRuntime>,
        request: RunRequest,
        message_id: String,
    ) -> Result<(), RpcError> {
        if message_id.trim().is_empty() || request.prompt.trim().is_empty() {
            return Err(RpcError::BadParams(
                "messageId and prompt must not be empty".into(),
            ));
        }
        {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            if chat.is_removed() {
                return Err(RpcError::Failed("chat was deleted".into()));
            }
            let attended = Self::attended_send(&chat, &queue);
            queue.enqueue(
                request,
                message_id,
                PendingKind::Ordinary,
                None,
                None,
                attended,
            )?;
        }
        self.kick_queue(chat);
        Ok(())
    }

    async fn queue_command(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: QueueCommandParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if !crate::store::id_is_path_safe(&params.chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        let chat = self.runtime.chat(&params.chat_id);
        match params.command {
            SessionCommandPayload::Interrupt {} => {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if chat.is_removed() {
                    return Err(RpcError::Failed("chat was deleted".into()));
                }
                let paused = queue.pause(true);
                if let Some(cancel) = chat
                    .cancel
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    cancel.cancel();
                }
                paused?;
            }
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                self.enqueue_run(chat, request, message_id)?;
            }
            // The dedicated skill invocation command is retired (ADR-0035):
            // skills ride ordinary messages as inline `$` mentions. The
            // payload stays deserializable for old command ledgers.
            SessionCommandPayload::InvokeSkill { .. } => {
                return Err(RpcError::Failed(
                    "skill invocations are inline $ mentions now — send the text as an ordinary message".into(),
                ));
            }
            SessionCommandPayload::Steer {
                prompt,
                message_id,
                request,
            } => {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(id) = message_id {
                    queue.promote(&id)?;
                } else {
                    let mut request = request
                        .ok_or_else(|| RpcError::BadParams("steer request is required".into()))?;
                    if prompt.trim().is_empty() {
                        return Err(RpcError::BadParams("prompt must not be empty".into()));
                    }
                    request.prompt = prompt;
                    queue.enqueue_priority(request, uuid::Uuid::new_v4().to_string())?;
                }
                queue.pause(false)?;
                if let Some(cancel) = chat
                    .cancel
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    cancel.cancel();
                }
                drop(queue);
                self.kick_queue(chat);
            }
            SessionCommandPayload::RespondInput { .. } => {
                return Err(RpcError::Failed(
                    "input responses are not available yet".into(),
                ));
            }
            SessionCommandPayload::Compact {
                request,
                message_id,
            } => {
                // A manual Compaction joins the same ordered queue (ADR-0011
                // as amended by message-queue ticket 04): the driver admits
                // it in submission order and runs it outside the Turn model.
                // A pre-queue peer sends no id — mint one (no dedup possible
                // for its retries, same as any id-less command).
                let message_id = if message_id.trim().is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    message_id
                };
                {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    // Manual Compaction keeps its strict submission order
                    // (ADR-0011): an attended /compact parks like any
                    // queued item, and Continue is its way forward.
                    queue.enqueue(request, message_id, PendingKind::Compact, None, None, false)?;
                }
                self.kick_queue(chat);
            }
        }
        RpcReply::value(&serde_json::json!({}))
    }

    /// Accept and launch one queued Turn — the driver's tail for ordinary
    /// messages. `parts` is the transcript user entry (the prompt text),
    /// `preview` the sidebar/title text, `prompt` the model-visible text —
    /// rebuilt at the admission checkpoint from the item the queue holds
    /// NOW, so an edit that landed mid-pick wins. Inline `$` skill mentions
    /// resolve here too (ADR-0035): every resolved mention's `<skill>` block
    /// is prepended to the prompt and seeded as the head of the run's own
    /// entry.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_turn(
        &self,
        chat_id: &str,
        chat: Arc<ChatRuntime>,
        request: RunRequest,
        message_id: String,
        mut parts: Vec<MessagePart>,
        mut preview: String,
        mut prompt: String,
        mut title_prompt: Option<String>,
        cancel: CancellationToken,
        queued: bool,
    ) -> Result<AgentRun, RpcError> {
        if let Some(error) = chat
            .persistence_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Err(RpcError::Failed(format!(
                "Conversation could not be saved ({error}). Restore storage and reopen Holt before continuing."
            )));
        }
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
        let now = Utc::now();
        let timestamp = now.timestamp_millis().max(
            chat.transcript
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .rev()
                .find(|entry| entry.role == MessageRole::User)
                .map_or(0, |entry| entry.created_at.saturating_add(1)),
        );
        if cancel.is_cancelled() || chat.is_removed() {
            return Err(RpcError::Failed("Turn interrupted before execution".into()));
        }
        // The queued admission checkpoint: persist the pending-to-started
        // transition before anything Turn-shaped is built. The admitted item
        // comes back so an edit that landed between the queue pick and this
        // checkpoint wins — the Turn is built from the body the queue holds
        // now, not from the pick-time snapshot.
        let mut invocation: Vec<MessagePart> = Vec::new();
        if queued {
            let admitted = {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() || chat.is_removed() {
                    return Err(RpcError::Failed("Turn interrupted before execution".into()));
                }
                queue.start(&message_id, timestamp)?
            };
            match admitted.message.kind {
                PendingKind::Ordinary | PendingKind::Skill => {
                    // Pending Skill items no longer exist (load-time
                    // migration, ADR-0035); the arm stays for the enum.
                    let current = admitted.message.request.prompt;
                    if current != prompt {
                        prompt = current.clone();
                        preview = current.clone();
                        // The queued entry's transcript shape is exactly one text part.
                        parts = vec![MessagePart::Text {
                            id: "t0".into(),
                            text: current.clone(),
                        }];
                        title_prompt = Some(current);
                    }
                    // Inline mentions resolve against a fresh catalog at
                    // admission: resolved `<skill>` blocks prepend the
                    // model-visible prompt and seed the run entry's opening
                    // chips; unresolved mentions stay ordinary text.
                    let (model_prompt, chips) = self
                        .skills
                        .resolve_prompt_mentions(&request.cwd, &prompt)
                        .await;
                    prompt = model_prompt;
                    invocation = chips;
                }
                // Manual Compaction is admitted by the driver itself — it
                // never becomes a Turn (ADR-0011).
                PendingKind::Compact => unreachable!("Compaction never enters start_turn"),
            }
        }
        let baseline = self.git.turn_baseline(&request.cwd).await.ok();
        // Resolve live checkout identity only when this message reaches
        // admission. A pending message does not own a Turn baseline.
        let source = self
            .git
            .turn_source_context(&request.cwd, &self.engine_info.device_id)
            .await;
        // Title task (ADR-0012): resolve its inputs only when the row could
        // still be eligible, so later prompts never touch title settings.
        // Every failure here is silent — a missing or invalid title model
        // must never fail the Turn.
        let mut title_spec = if title_prompt.is_some() && self.title_may_be_eligible(chat_id, &chat)
        {
            self.prepare_title_task(chat_id, &chat, title_prompt.as_deref().unwrap_or_default())
                .await
        } else {
            None
        };

        // No runtime-wide lock spans admission (ADR-0032): the registry
        // write serializes on `chats_store`, the user entry on the chat's
        // own persistence lock, and the driver's per-chat execution mutex
        // already orders this chat's runs.
        let mut title_spawn = None;
        // The Turn's mode snapshot (ADR-0014): the stored mode, or the
        // sticky default for a row without a config yet. Taken at
        // acceptance — a switch after this point affects only the next
        // Turn.
        let mut mode = self.mode_default.get();
        // The Turn's Plan Mode snapshot (ADR-0025): planning at admission
        // makes this a planning Turn; a mid-Turn switch lands from the
        // next Turn exactly like the mode beside it.
        let mut planning = false;
        // The Turn's Provider Mode snapshot (ADR-0037), taken at
        // acceptance like the permission mode and Plan Mode.
        let mut provider_mode = false;
        {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) {
                row.cwd = Some(request.cwd.clone());
                if let Some(source) = source {
                    row.branch = Some(source.branch.clone());
                    row.source_context = Some(source);
                }
                planning = row.plan_mode.is_some();
                // Legacy model-setup rows run as Provider Mode until they
                // are retired.
                let scope = row
                    .config
                    .as_ref()
                    .map(|config| config.scope)
                    .unwrap_or_default();
                provider_mode = row.provider_mode || scope == holt_proto::ChatScope::ModelSetup;
                // The permission mode is NOT the
                // request's to move (ADR-0014): the stored mode is
                // authoritative — switches land through the mode RPC and
                // take effect from the next Turn — and a row without a
                // config yet inherits the sticky default, exactly like a
                // newly created chat.
                mode = row
                    .config
                    .as_ref()
                    .map(|config| config.permission_mode)
                    .unwrap_or(mode);
                row.config = Some(ChatConfig {
                    provider: request.provider.clone(),
                    model: request.model.clone(),
                    reasoning: request.reasoning,
                    model_options: request.model_options.clone(),
                    permission_mode: mode,
                    scope,
                });
                row.last_message_preview = Some(preview.chars().take(120).collect());
                row.last_message_at = Some(now);
                if row.title.is_none() {
                    // The fallback skips the composer's path-list trailer
                    // — a references-only send must not title the chat
                    // "Referenced paths:".
                    row.title = Some(crate::title_task::first_line_title(&preview));
                    // The first prompt is the only eligibility window:
                    // stamp the one-shot marker in the same write as the
                    // fallback title, so a second prompt can never start a
                    // second task and a restart never retries.
                    if let Some(spec) = title_spec.take()
                        && row.title_source == TitleSource::Automatic
                        && !row.title_task_started
                    {
                        row.title_task_started = true;
                        title_spawn = Some((spec, row.created_at));
                    }
                }
            }
        }
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let had_baseline = baseline.is_some();
        if let Some(baseline) = baseline {
            self.turn_changes
                .begin(chat_id, &message_id, &request.cwd, baseline);
        }
        // The Turn's attribution recorder, resolved now that `begin` has
        // created the record: the run's tools record their write paths into
        // it, and the change set keeps only attributed files (another
        // chat's concurrent work in the same working tree must not land in
        // this Turn's card). A turn without a Git baseline has no change
        // set to filter — no recorder.
        let attribution = if had_baseline {
            self.turn_changes
                .attribution(chat_id, &message_id)
                .map(|set| crate::tools::ChangeAttribution::new(set, self.git.clone()))
        } else {
            None
        };
        let entry = SessionMessageEntry {
            id: message_id.clone(),
            role: MessageRole::User,
            parts,
            created_at: timestamp,
            device_id: self.engine_info.device_id.clone(),
            status: None,
            continuation_of: None,
        };
        let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = transcript.iter_mut().find(|entry| entry.id == message_id) {
            *existing = entry;
        } else {
            transcript.push(entry);
        }
        drop(transcript);
        self.runtime.publish_chats();
        self.runtime.set_session(chat_id, SessionStatus::Working);
        // Run acceptance rewrites the chat's config from the request, so the
        // chat's selection (the occupancy denominator's fallback) moves with
        // it.
        self.refresh_selected_model(chat_id);

        // The Title task runs in parallel with the Turn on its own token —
        // a Turn interrupt must not cancel it (only chat deletion does).
        if let Some((spec, generation)) = title_spawn {
            let token = CancellationToken::new();
            *chat.title_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
            tokio::spawn(crate::title_task::run_title_task(
                self.runtime.clone(),
                spec,
                generation,
                token,
            ));
        }
        // The admitted user entry lands in the log now, complete at
        // creation (ADR-0032) — not on the next run event.
        chat.persist_entry(&message_id);

        let runtime = self.runtime.clone();
        let chat_id = chat_id.to_string();
        Ok(AgentRun {
            runtime,
            chat_id,
            chat,
            prompt,
            cwd: request.cwd,
            reasoning: request.reasoning,
            model,
            api_key,
            timestamp,
            cancel,
            skills: self.skills.clone(),
            invocation,
            permission_mode: mode,
            plan_mode: planning,
            provider_mode,
            // The admission-time backend snapshot (ADR-0023): resolved
            // once here, so a settings change mid-Turn lands from the
            // next Turn — the same snapshot semantics as the mode.
            search_backend: self.search_backend(),
            providers: Some(Arc::clone(&self.providers)),
            attribution,
            stream_fn: self.runtime.stream_fn.clone(),
        })
    }

    fn set_chat_config(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let mut config: ChatConfig = serde_json::from_value(
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
        // Same rule as the run-acceptance write: the permission mode is not
        // this payload's to move (ADR-0014) — the stored mode survives
        // whole-config rewrites, and a config-less row inherits the sticky
        // default instead of whatever tier the writer defaulted.
        config.permission_mode = chat
            .config
            .as_ref()
            .map(|stored| stored.permission_mode)
            .unwrap_or_else(|| self.mode_default.get());
        chat.config = Some(config);
        persist_chats(&self.data_dir, &chats)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        drop(chats);
        self.runtime.publish_chats();
        // The picker moves the occupancy denominator: with an empty queue the
        // selection's window is what the next request is measured against.
        self.refresh_selected_model(chat_id);
        RpcReply::value(&serde_json::json!({}))
    }

    /// Re-seed every open chat's occupancy windows after a catalog write:
    /// the denominator map is seeded once per watch open, so a new or
    /// changed record would otherwise serve a stale window until the watch
    /// reopened.
    fn refresh_catalog_windows(&self) {
        for chat in self.runtime.open_chats() {
            let chat_id = chat.chat_id.clone();
            crate::usage::seed_occupancy(
                &chat,
                self.providers.context_windows().into_iter().collect(),
                self.selected_wire_model(&chat_id),
            );
            crate::usage::publish(&chat);
        }
    }

    /// Replace the latest user message and run it again. Drafting stays in
    /// the UI; submission is the serialization point that cancels the old
    /// Turn, prunes both records, and requeues the replacement ahead of any
    /// remaining work.
    async fn edit_last_message(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let message_id = required_string(&params, "messageId")?;
        let prompt = required_string(&params, "prompt")?.to_string();
        if !crate::store::id_is_path_safe(chat_id) {
            return Err(RpcError::BadParams("invalid chatId".into()));
        }
        if prompt.trim().is_empty() {
            return Err(RpcError::BadParams("prompt must not be empty".into()));
        }
        let chat = self.runtime.chat(chat_id);

        let (target_index, target) = {
            let transcript = chat.transcript.read().unwrap_or_else(|e| e.into_inner());
            let Some((index, target)) = transcript
                .iter()
                .enumerate()
                .rev()
                .find(|(_, entry)| entry.role == MessageRole::User)
            else {
                return Err(RpcError::Failed("chat has no user message to edit".into()));
            };
            if target.id != message_id {
                return Err(RpcError::Failed(
                    "only the latest user message can be edited".into(),
                ));
            }
            (index, target.clone())
        };

        // Edits are ordinary messages now — skill mentions in the new text
        // resolve at admission like any send (ADR-0035). A legacy Skill-part
        // entry edits the same way; its replacement is the typed text.
        let (request, attended) = {
            let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
            let row = chats
                .iter()
                .find(|row| row.id == chat_id)
                .ok_or_else(|| RpcError::Failed("unknown chat".into()))?;
            let config = row
                .config
                .clone()
                .ok_or_else(|| RpcError::Failed("chat has no run configuration".into()))?;
            let cwd = row
                .cwd
                .clone()
                .ok_or_else(|| RpcError::Failed("chat has no working directory".into()))?;
            let request = RunRequest {
                prompt: prompt.clone(),
                provider: config.provider,
                model: config.model,
                reasoning: config.reasoning,
                model_options: config.model_options,
                cwd,
                permission_mode: config.permission_mode,
                auto_approve: false,
                attachments: Vec::new(),
                worktree: None,
            };
            let attended = {
                let queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.paused()
            };
            (request, attended)
        };

        // Pause before cancelling so a pending item cannot be admitted while
        // the old Turn is unwinding. The execution mutex is released only
        // after queue settlement and History repair have completed.
        {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.pause(true)?;
        }
        if let Some(cancel) = chat
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            cancel.cancel();
        }
        let _execution = chat.execution.lock().await;

        // The pause parks ordinary sends, but an attended send admitted
        // before the pause, a second edit request, or another device may
        // have changed the tail — re-validate under the execution lock.
        let entries = {
            let transcript = chat.transcript.read().unwrap_or_else(|e| e.into_inner());
            let Some((latest_index, latest)) = transcript
                .iter()
                .enumerate()
                .rev()
                .find(|(_, entry)| entry.role == MessageRole::User)
            else {
                self.resume_after_edit_failure(&chat, attended);
                return Err(RpcError::Failed("chat has no user message to edit".into()));
            };
            if latest_index != target_index || latest.id != message_id {
                self.resume_after_edit_failure(&chat, attended);
                return Err(RpcError::Failed(
                    "only the latest user message can be edited".into(),
                ));
            }
            let mut replacement = latest.clone();
            let mut replaced = false;
            for part in &mut replacement.parts {
                if let MessagePart::Text { text, .. } = part {
                    if !replaced {
                        *text = prompt.clone();
                        replaced = true;
                    } else {
                        *text = String::new();
                    }
                }
            }
            if !replaced {
                replacement.parts.push(MessagePart::Text {
                    id: "t0".into(),
                    text: prompt.clone(),
                });
            }
            let mut entries = transcript[..=latest_index].to_vec();
            entries[latest_index] = replacement.clone();
            entries
        };

        // Persistence first, then the records — the same nesting the run
        // path settles under (ADR-0032), so repair cannot interleave with
        // a settle.
        let _persistence = chat.persistence.lock().unwrap_or_else(|e| e.into_inner());
        let mut history = chat.history.write().unwrap_or_else(|e| e.into_inner());
        let history_index = history
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| {
                matches!(message, pi_core::agent::types::AgentMessage::User(user) if user.timestamp == target.created_at)
            })
            .map(|(index, _)| index);
        let Some(history_index) = history_index else {
            drop(history);
            self.resume_after_edit_failure(&chat, attended);
            return Err(RpcError::Failed(
                "the message is not present in chat history".into(),
            ));
        };
        history.truncate(history_index);
        if let Err(error) = crate::store::rewrite_transcript(&self.data_dir, chat_id, &entries) {
            drop(history);
            self.resume_after_edit_failure(&chat, attended);
            return Err(RpcError::Failed(error.to_string()));
        }
        if let Err(error) = crate::history::rewrite(&self.data_dir, chat_id, &history) {
            drop(history);
            self.resume_after_edit_failure(&chat, attended);
            return Err(RpcError::Failed(error.to_string()));
        }
        drop(history);
        *chat.transcript.write().unwrap_or_else(|e| e.into_inner()) = entries;
        // The log was rewritten whole: the incremental writers' anchors are
        // gone, so every entry's next persist is a full line again.
        chat.clear_persisted_parts();
        chat.publish();

        // Requeue outside the queue lock: the failure path re-locks the
        // queue to restore its pre-edit state.
        let enqueue = {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.enqueue_replacement(
                request,
                message_id.to_string(),
                PendingKind::Ordinary,
                None,
                None,
                attended,
            )
        };
        if let Err(error) = enqueue {
            self.resume_after_edit_failure(&chat, attended);
            return Err(error);
        }
        drop(_execution);
        drop(_persistence);
        self.kick_queue(chat);
        RpcReply::value(&serde_json::json!({ "messageId": message_id }))
    }

    /// Undo the edit's pause on a failure path: a queue the user had
    /// already parked stays parked; only a queue that was running before
    /// the edit resumes.
    fn resume_after_edit_failure(&self, chat: &Arc<ChatRuntime>, was_paused: bool) {
        if !was_paused {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            let _ = queue.resume();
        }
        self.kick_queue(chat.clone());
    }

    /// The chat's selected model as the occupancy windows are keyed — the
    /// denominator `WatchChatUsage` falls back to when the queue holds
    /// nothing next. `None` for a row without a config yet (a chat that has
    /// never run has no selection to divide by).
    fn selected_wire_model(&self, chat_id: &str) -> Option<String> {
        let chats = self.runtime.chats.read().ok()?;
        let config = chats
            .iter()
            .find(|row| row.id == chat_id)?
            .config
            .as_ref()?;
        Some(crate::usage::wire_model_id(
            &config.provider.0,
            &config.model,
        ))
    }

    /// Point an OPEN chat runtime's denominator at its current selection.
    /// Cheap and idempotent: an unopened chat has no watch to correct yet —
    /// the subscription seeds it from the same source.
    fn refresh_selected_model(&self, chat_id: &str) {
        if let Some(chat) = self.runtime.loaded_chat(chat_id) {
            crate::usage::set_selected_model(&chat, self.selected_wire_model(chat_id));
        }
    }

    /// Switch a chat's permission mode (ADR-0014): the stored mode is the
    /// single source of truth a Turn snapshots at start, so the switch
    /// takes effect from the next Turn. The choice also becomes the
    /// device's sticky default for new chats. A chat without a config yet
    /// only moves the sticky default — its first run seeds the config from
    /// that default.
    fn set_chat_permission_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        // Strict at the RPC boundary, unlike the lenient stored-value
        // decode: a typo'd tier must fail loudly here, not silently become
        // the confirm-changes default (and the sticky record with it).
        let raw_mode = required_string(&params, "mode")?;
        let mode: holt_proto::PermissionMode = serde_json::from_value(serde_json::json!(raw_mode))
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        if !matches!(
            raw_mode,
            "confirm-changes"
                | "auto-review"
                | "full-access"
                | "workspace-write"
                | "read-only"
                | "danger-full-access"
        ) {
            return Err(RpcError::BadParams(format!(
                "unknown permission mode: {raw_mode}"
            )));
        }
        {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            if let Some(config) = chat.config.as_mut() {
                config.permission_mode = mode;
            }
            persist_chats(&self.data_dir, &chats)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        self.runtime.publish_chats();
        // The sticky default is best-effort after the chat's own mode
        // landed: a failed write costs only future chats' inheritance, not
        // this switch — same philosophy as the transcript snapshot.
        if let Err(error) = self.mode_default.save(mode) {
            tracing::warn!(target: "holt::agent", %error, "could not persist the permission-mode default");
        }
        RpcReply::value(&serde_json::json!({ "mode": mode }))
    }

    /// Resolve a pending confirm-changes Approval (ADR-0014): the verdict
    /// releases the gate the run is blocked in — allow executes the call,
    /// always-allow executes it and records the session grant, deny blocks
    /// it with the note (or the standard denial) as the reason the model
    /// reads, and the Turn continues either way.
    fn resolve_approval(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let approval_id = required_string(&params, "approvalId")?;
        let verdict: holt_proto::ApprovalVerdict = serde_json::from_value(
            params
                .get("verdict")
                .cloned()
                .ok_or_else(|| RpcError::BadParams("verdict is required".into()))?,
        )
        .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let pending = self
            .runtime
            .approvals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(approval_id);
        let Some(pending) = pending else {
            return Err(RpcError::Failed(
                "unknown or already-resolved approval".into(),
            ));
        };
        // A dropped receiver means the Turn ended between the registry hit
        // and the send (interrupt raced the verdict) — the call already
        // settled as aborted.
        pending
            .send(verdict)
            .map_err(|_| RpcError::Failed("the approval's Turn already ended".into()))?;
        RpcReply::value(&serde_json::json!({}))
    }

    /// Enter Plan Mode (ADR-0025): record the chat's CURRENT permission
    /// mode as the entry mode — restored on plan approval, never moved by
    /// the entry itself — and mark the chat planning. Idempotent: an
    /// already-planning chat replies its state unchanged. A chat without a
    /// config yet inherits the sticky default, exactly as its first Turn
    /// would.
    fn enter_plan_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        self.set_provider_mode(chat_id, false)?;
        {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            if chat.plan_mode.is_none() {
                let entry_mode = chat
                    .config
                    .as_ref()
                    .map(|config| config.permission_mode)
                    .unwrap_or_else(|| self.mode_default.get());
                chat.plan_mode = Some(holt_proto::ChatPlanState {
                    entry_permission_mode: entry_mode,
                });
                persist_chats(&self.data_dir, &chats)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
        }
        self.runtime.publish_chats();
        RpcReply::value(&self.plan_mode_state(chat_id)?)
    }

    /// Leave Plan Mode (ADR-0025). Idempotent. The current permission mode
    /// stands — only plan approval restores the entry mode — and pending
    /// approval cards settle as dismissed, never answerable for a chat
    /// that stopped planning.
    fn exit_plan_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let exited = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            let exited = chat.plan_mode.take().is_some();
            if exited {
                persist_chats(&self.data_dir, &chats)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
            exited
        };
        self.runtime.publish_chats();
        if exited && let Some(chat) = self.runtime.loaded_chat(chat_id) {
            crate::plan_mode::settle_plan_cards(
                &chat,
                holt_doc::parts::PlanApprovalVerdict::Dismissed,
            );
        }
        RpcReply::value(&self.plan_mode_state(chat_id)?)
    }

    /// Enter Provider Mode (ADR-0037). Idempotent; a planning chat leaves
    /// Plan Mode first — the two modes never overlap.
    fn enter_provider_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        self.exit_plan_mode(serde_json::json!({ "chatId": chat_id }))?;
        self.set_provider_mode(chat_id, true)?;
        RpcReply::value(&self.provider_mode_state(chat_id)?)
    }

    /// Leave Provider Mode (ADR-0037). Idempotent. Pending proposal and key
    /// cards stay writable: they are the user's to settle, not the mode's.
    fn exit_provider_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        self.set_provider_mode(chat_id, false)?;
        RpcReply::value(&self.provider_mode_state(chat_id)?)
    }

    /// Persist and broadcast the chat row's Provider Mode flag when it
    /// moves.
    fn set_provider_mode(&self, chat_id: &str, active: bool) -> Result<(), RpcError> {
        let changed = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            let changed = chat.provider_mode != active;
            if changed {
                chat.provider_mode = active;
                persist_chats(&self.data_dir, &chats)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
            }
            changed
        };
        if changed {
            self.runtime.publish_chats();
        }
        Ok(())
    }

    fn provider_mode_state(
        &self,
        chat_id: &str,
    ) -> Result<holt_proto::ProviderModeState, RpcError> {
        let chats = self
            .runtime
            .chats
            .read()
            .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
        let chat = chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        Ok(holt_proto::ProviderModeState {
            active: chat.provider_mode,
        })
    }

    /// Resolve a proposed plan (ADR-0025): the verdict applies to the
    /// chat's Plan Mode. Approve exits Plan Mode restoring the entry
    /// permission mode and enqueues the approval follow-up prompt as an
    /// ordinary run — the plan is already in the conversation History, so
    /// the implementation Turn carries it naturally and starts on its own;
    /// reject keeps the chat planning and a non-empty feedback is enqueued
    /// as the revision loop's next planning input; remain changes nothing
    /// but the cards. Every verdict requires a planning chat with at least
    /// one pending card, and settles ALL pending cards (they address the
    /// same checkpoint).
    fn resolve_plan_approval(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let verdict = required_string(&params, "verdict")?;
        let feedback = optional_string(&params, "feedback");
        let decision = match verdict {
            "approve" => Decision::Approve,
            "reject" => Decision::Reject,
            "remain" => Decision::Remain,
            other => {
                return Err(RpcError::BadParams(format!(
                    "unknown plan verdict: {other}; expected approve, reject, or remain"
                )));
            }
        };
        let lifecycle = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            let Some(plan_state) = chat.plan_mode.as_mut() else {
                return Err(RpcError::Failed("this chat is not in Plan Mode".into()));
            };
            if !crate::plan_mode::has_pending_plan_cards(chat_id, &self.runtime) {
                return Err(RpcError::Failed("no plan is awaiting approval".into()));
            }
            match decision {
                // Restore the entry permission mode (ADR-0025): the stored
                // mode moves back, the sticky default is untouched — this
                // is a restore, not a choice. The plan is already in the
                // conversation History; there is nothing to inject.
                Decision::Approve => {
                    if let Some(config) = chat.config.as_mut() {
                        config.permission_mode = plan_state.entry_permission_mode;
                    }
                    chat.plan_mode = None;
                    persist_chats(&self.data_dir, &chats)
                        .map_err(|error| RpcError::Failed(error.to_string()))?;
                    Lifecycle::Approved
                }
                // The chat keeps planning; the next planning Turn (the
                // feedback, enqueued below) proposes a replacement block.
                // The stored mode stands — only approval restores.
                Decision::Reject => {
                    persist_chats(&self.data_dir, &chats)
                        .map_err(|error| RpcError::Failed(error.to_string()))?;
                    Lifecycle::Rejected
                }
                Decision::Remain => Lifecycle::Remained,
            }
        };
        self.runtime.publish_chats();
        if let Some(chat) = self.runtime.loaded_chat(chat_id) {
            crate::plan_mode::settle_plan_cards(
                &chat,
                match lifecycle {
                    Lifecycle::Approved => holt_doc::parts::PlanApprovalVerdict::Approved,
                    Lifecycle::Rejected => holt_doc::parts::PlanApprovalVerdict::Rejected,
                    Lifecycle::Remained => holt_doc::parts::PlanApprovalVerdict::Remained,
                },
            );
            match lifecycle {
                // The approval speaks as an ordinary user message: the
                // follow-up run opens the implementation Turn, which reads
                // the plan from History. The config was just restored to
                // the entry mode, so the run carries it.
                Lifecycle::Approved => {
                    self.enqueue_plan_follow_up(&chat, crate::plan_mode::APPROVAL_FOLLOW_UP_PROMPT)
                }
                Lifecycle::Rejected => {
                    if let Some(feedback) = feedback {
                        self.enqueue_plan_follow_up(&chat, &feedback);
                    }
                }
                Lifecycle::Remained => {}
            }
        }
        RpcReply::value(&self.plan_mode_state(chat_id)?)
    }

    /// A plan verdict's follow-up run: the approval's consent prompt or
    /// the rejection feedback as the revision loop's next planning input —
    /// an ordinary queued run using the chat's captured model settings.
    /// Best-effort — a chat without a captured config or working directory
    /// (nothing was ever planned) records a warning instead.
    fn enqueue_plan_follow_up(&self, chat: &Arc<crate::agent::ChatRuntime>, prompt: &str) {
        let request = {
            let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
            let Some(row) = chats.iter().find(|row| row.id == chat.chat_id) else {
                return;
            };
            let Some(config) = row.config.as_ref() else {
                tracing::warn!(target: "holt::agent", "plan follow-up dropped: the chat has no captured model settings");
                return;
            };
            let Some(cwd) = row.cwd.clone() else {
                tracing::warn!(target: "holt::agent", "plan follow-up dropped: the chat has no working directory");
                return;
            };
            Self::queued_run_request(config, prompt, cwd)
        };
        if let Err(error) =
            self.enqueue_run(chat.clone(), request, uuid::Uuid::new_v4().to_string())
        {
            tracing::warn!(target: "holt::agent", %error, "could not enqueue the plan follow-up")
        }
    }

    /// The `GetPlanMode` view: whether the chat is planning and its
    /// recorded entry mode.
    fn plan_mode_state(&self, chat_id: &str) -> Result<holt_proto::PlanModeState, RpcError> {
        let chats = self
            .runtime
            .chats
            .read()
            .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
        let chat = chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        Ok(match &chat.plan_mode {
            Some(state) => holt_proto::PlanModeState {
                active: true,
                entry_permission_mode: Some(state.entry_permission_mode),
            },
            None => holt_proto::PlanModeState {
                active: false,
                entry_permission_mode: None,
            },
        })
    }

    async fn title_settings_state(&self) -> TitleSettingsState {
        let settings = self.title_settings.get();
        let warning = self.title_settings_warning(&settings).await;
        TitleSettingsState { settings, warning }
    }

    /// Missing credentials are a visible warning, never an error: the title
    /// task fails silently and normal chat Turns are unaffected.
    async fn title_settings_warning(&self, settings: &TitleSettings) -> Option<String> {
        let model_id = settings.model_id.as_deref()?;
        let provider = model_id.split('/').next()?;
        if self
            .providers
            .credentials
            .reveal_key(provider)
            .await
            .is_some()
        {
            return None;
        }
        Some(format!(
            "Provider {provider} has no saved credentials — automatic titles will keep the fallback until a key is configured."
        ))
    }

    /// The web-search settings view (ADR-0023) — the reply shape of the
    /// read and save RPCs. The raw key never rides this view.
    fn web_search_state(&self) -> WebSearchSettingsState {
        let backends = crate::tools::web_search::BACKENDS
            .iter()
            .map(|backend| WebSearchBackendOption {
                id: backend.id.to_string(),
                name: backend.name.to_string(),
                note: backend.note.map(str::to_string),
            })
            .collect();
        match self.web_search.get() {
            Some(record) => WebSearchSettingsState {
                backend: Some(record.backend),
                api_key_masked: Some(masked_key(&record.api_key)),
                backends,
            },
            None => WebSearchSettingsState {
                backend: None,
                api_key_masked: None,
                backends,
            },
        }
    }

    /// The Jev settings view (ADR-0027) — the reply shape of the read and
    /// save RPCs. The raw key never rides this view.
    fn jev_state(&self) -> JevSettingsState {
        JevSettingsState {
            api_key_masked: self.jev.get().map(|record| masked_key(&record.api_key)),
        }
    }

    async fn save_jev_settings(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let key = required_string(&params, "apiKey")?;
        self.jev
            .save(key)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.jev_state())
    }

    /// The MCP settings view (ADR-0034): every definition — flat,
    /// camelCase, exactly the hand-editable `mcpServers` entry shape —
    /// plus any file-level validation error from a hand edit that landed
    /// since startup (a broken file keeps the last-good set).
    async fn mcp_settings_state(&self) -> serde_json::Value {
        let validation_error = self.runtime.mcp.refresh_error();
        let servers: Vec<serde_json::Value> = self
            .runtime
            .mcp
            .definitions()
            .into_iter()
            .map(|(name, server)| {
                let mut value = crate::mcp::config::server_to_value(&server);
                let object = value.as_object_mut().unwrap();
                object.insert("name".into(), serde_json::json!(name));
                // The UI view carries the shared fields explicitly, even
                // when the file form omits defaults.
                object.insert("enabled".into(), serde_json::json!(server.enabled));
                object.insert(
                    "startupTimeoutMs".into(),
                    serde_json::json!(server.startup_timeout_ms),
                );
                object.insert(
                    "toolTimeoutMs".into(),
                    serde_json::json!(server.tool_timeout_ms),
                );
                object.insert(
                    "enabledTools".into(),
                    serde_json::json!(server.enabled_tools),
                );
                object.insert(
                    "disabledTools".into(),
                    serde_json::json!(server.disabled_tools),
                );
                value
            })
            .collect();
        serde_json::json!({
            "servers": servers,
            "validationError": validation_error,
        })
    }

    /// `SaveMcpServer` — the strict upsert. The name must satisfy
    /// `[A-Za-z0-9_-]` (the two-level tool naming never becomes
    /// ambiguous) and the definition survives the same strict parse a
    /// hand-edited file would; the write is atomic under the credentials
    /// pattern, and the server's cached connection is dropped so the
    /// next Turn serves the new definition.
    async fn save_mcp_server(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let name = required_string(&params, "name")?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(RpcError::BadParams(format!(
                "invalid mcp server name {name:?}: names use [A-Za-z0-9_-] only"
            )));
        }
        let definition = params
            .get("server")
            .cloned()
            .ok_or_else(|| RpcError::BadParams("server is required".into()))?;
        let server =
            crate::mcp::config::parse_server(name, &definition).map_err(RpcError::BadParams)?;
        let mut servers = self.runtime.mcp.definitions();
        servers.insert(name.to_string(), server);
        self.runtime
            .mcp
            .store()
            .save(servers)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.mcp.invalidate(name).await;
        RpcReply::value(&self.mcp_settings_state().await)
    }

    /// `RemoveMcpServer` — delete one definition (its cached connection
    /// dies with it).
    async fn remove_mcp_server(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let name = required_string(&params, "name")?;
        let mut servers = self.runtime.mcp.definitions();
        if servers.remove(name).is_none() {
            return Err(RpcError::BadParams(format!("unknown mcp server {name:?}")));
        }
        self.runtime
            .mcp
            .store()
            .save(servers)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.mcp.invalidate(name).await;
        RpcReply::value(&self.mcp_settings_state().await)
    }

    /// `TestMcpServer` — the probe reply: status, tool count and names,
    /// or the failure reason. Reads the current definitions without
    /// disturbing the pool's live connections.
    async fn mcp_probe_reply(&self, name: &str) -> serde_json::Value {
        match self.runtime.mcp.probe(name).await {
            crate::mcp::ProbeReport::Ok {
                tool_count,
                tool_names,
            } => serde_json::json!({
                "status": "ok",
                "toolCount": tool_count,
                "toolNames": tool_names,
            }),
            crate::mcp::ProbeReport::Failed { reason } => {
                serde_json::json!({ "status": "failed", "reason": reason })
            }
        }
    }

    async fn save_web_search_settings(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let backend = required_string(&params, "backend")?;
        let key = required_string(&params, "apiKey")?;
        let known = crate::tools::web_search::BACKENDS;
        if !known.iter().any(|entry| entry.id == backend) {
            return Err(RpcError::BadParams(format!(
                "unknown search backend {backend:?}; expected one of {}",
                known
                    .iter()
                    .map(|entry| entry.id)
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
        self.web_search
            .save(backend, key)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.web_search_state())
    }

    /// The Turn's web-search backend (ADR-0023), resolved once per Turn
    /// admission — a mid-Turn settings change lands from the next Turn,
    /// like the permission mode. Unconfigured — or an id whose adapter
    /// slice has not landed — resolves to no backend, so `web_search`
    /// stays out of the toolset.
    fn search_backend(&self) -> Option<Arc<dyn crate::SearchBackend>> {
        let record = self.web_search.get()?;
        match &self.search_backend_resolver {
            Some(resolve) => resolve(&record.backend),
            None => crate::tools::web_search::builtin(&record.backend, &record.api_key),
        }
    }

    /// Cheap read-side pre-check for the Title task's eligibility window:
    /// an untitled, automatically-owned chat whose one-shot task has not
    /// started. Only a first prompt can satisfy this — the fallback title
    /// is stamped in the same acceptance pass.
    fn title_may_be_eligible(&self, chat_id: &str, chat: &ChatRuntime) -> bool {
        let has_user_prompt = chat
            .transcript
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|entry| entry.role == MessageRole::User)
            || chat
                .history
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|message| message.role() == "user");
        if has_user_prompt {
            return false;
        }
        let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
        chats.iter().any(|row| {
            row.id == chat_id
                && row.title.is_none()
                && row.title_source == TitleSource::Automatic
                && !row.title_task_started
        })
    }

    /// Resolve the Title task's inputs from the current settings. `None`
    /// means no task: automatic titles disabled, an unresolvable model, or
    /// missing credentials — all silent, the fallback title stays.
    async fn prepare_title_task(
        &self,
        chat_id: &str,
        chat: &Arc<crate::agent::ChatRuntime>,
        prompt: &str,
    ) -> Option<crate::title_task::TitleTaskSpec> {
        let settings = self.title_settings.get();
        let model_id = settings.model_id?;
        let (provider, _) = model_id.split_once('/')?;
        if !self.providers.is_eligible(provider) {
            return None;
        }
        let model = self.providers.resolve_model(provider, &model_id).ok()?;
        let api_key = self.providers.credentials.reveal_key(provider).await?;
        Some(crate::title_task::TitleTaskSpec {
            chat_id: chat_id.to_string(),
            data_dir: self.data_dir.clone(),
            // The chat whose ledger bills the round-trip; the run's own
            // runtime handle, so the title record lands on the live totals.
            chat: chat.clone(),
            prompt: prompt.to_string(),
            instruction: settings.instruction,
            model,
            api_key,
            stream_fn: self.runtime.stream_fn.clone(),
        })
    }

    async fn save_title_settings(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let mut settings: TitleSettings = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        // An empty/whitespace model id is the disabled state, not an error.
        settings.model_id = settings
            .model_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        if let Some(model_id) = settings.model_id.as_deref() {
            let Some((provider, _)) = model_id.split_once('/') else {
                return Err(RpcError::BadParams(format!(
                    "model must use provider/model syntax: {model_id}"
                )));
            };
            if !self.providers.is_eligible(provider) {
                return Err(RpcError::BadParams(format!(
                    "unknown or unsupported provider: {provider}"
                )));
            }
            self.providers
                .resolve_model(provider, model_id)
                .map_err(RpcError::BadParams)?;
        }
        let instruction = settings.instruction.trim();
        if instruction.is_empty() {
            return Err(RpcError::BadParams("instruction must not be empty".into()));
        }
        if instruction.chars().count() > MAX_TITLE_INSTRUCTION_CHARS {
            return Err(RpcError::BadParams(format!(
                "instruction must be at most {MAX_TITLE_INSTRUCTION_CHARS} characters"
            )));
        }
        settings.instruction = instruction.to_string();
        self.title_settings
            .save(settings)
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&self.title_settings_state().await)
    }
}

/// The `ResolvePlanApproval` verdicts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Approve,
    Reject,
    Remain,
}

/// What one resolution did to the chat's plan lifecycle — the card verdict
/// and the feedback enqueue both key off it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Approved,
    Rejected,
    Remained,
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
struct DeleteSpaceParams {
    space_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameSpaceParams {
    space_id: String,
    name: String,
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
struct SetChatPinnedParams {
    chat_id: String,
    pinned: bool,
}

/// `UsageStats`'s only parameter; the value itself is validated against
/// the offered ranges (7 | 30 | 90) at dispatch.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageStatsParams {
    days: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteChatParams {
    chat_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameChatParams {
    chat_id: String,
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchFilesParams {
    query: String,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
}

/// Selector + path shape shared by the File-sidebar workspace methods.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspacePathParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveWorkspaceFileParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    text: String,
    /// The disk version token the draft was based on.
    version: String,
    /// A confirmed overwrite's reviewed disk token (ticket 04): when set,
    /// the save applies only if the disk STILL holds exactly that version.
    #[serde(default)]
    expect_disk_version: Option<String>,
    #[serde(default)]
    bom: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    parent_path: Option<String>,
    name: String,
    #[serde(default)]
    is_dir: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    new_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    #[serde(default)]
    destination_directory: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrashEntryParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileAsParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    path: String,
    text: String,
    #[serde(default)]
    bom: bool,
}

impl WorkspacePathParams {
    /// Exactly one of chatId/spaceId, mirroring `SearchFiles`.
    fn check_selector(&self) -> Result<(), RpcError> {
        if self.chat_id.is_some() == self.space_id.is_some() {
            return Err(RpcError::BadParams(
                "exactly one of chatId or spaceId is required".into(),
            ));
        }
        Ok(())
    }

    /// The non-empty path the read family (text and image) requires.
    fn require_path(&self) -> Result<String, RpcError> {
        self.path
            .clone()
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| RpcError::BadParams("path is required".into()))
    }

    fn as_search_root(&self) -> SearchFilesParams {
        SearchFilesParams {
            query: String::new(),
            chat_id: self.chat_id.clone(),
            space_id: self.space_id.clone(),
        }
    }
}

fn required_string<'a>(params: &'a serde_json::Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{field} is required")))
}

/// An optional string param: blank counts as absent.
fn optional_string(params: &serde_json::Value, field: &str) -> Option<String> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

/// Mask a stored settings key (search or Jev) for display: the first and
/// last four characters joined by an ellipsis. At least one character must
/// stay hidden, so keys of eight or fewer characters reveal nothing at all.
fn masked_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn required_string_list(params: &serde_json::Value, field: &str) -> Result<Vec<String>, RpcError> {
    params
        .get(field)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .ok_or_else(|| RpcError::BadParams(format!("{field} must be a list of strings")))
}

/// Report git capture faults at the right RPC severity: caller-input
/// problems are bad params, repository failures are opaque errors.
fn git_fault(fault: crate::git::GitFault) -> RpcError {
    match fault {
        crate::git::GitFault::BadParams(message) => RpcError::BadParams(message),
        crate::git::GitFault::Error(message) => RpcError::Failed(message),
    }
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
            methods::OPEN_TERMINAL => {
                let params: holt_rpc::terminals::OpenTerminal = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                let known_chat = self
                    .runtime
                    .chats
                    .read()
                    .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?
                    .iter()
                    .any(|chat| chat.id == params.chat_id);
                // Chat-owned PTYs root at the chat's working directory (the
                // existing resolution, errors included). Chat-less terminals
                // — the new-chat canvas or the no-project empty state — take
                // the caller's explicit cwd, else the user's home directory.
                let cwd = if known_chat {
                    self.search_files_root(&SearchFilesParams {
                        chat_id: Some(params.chat_id.clone()),
                        space_id: None,
                        query: String::new(),
                    })?
                } else {
                    chatless_terminal_root(params.cwd.clone())?
                };
                let terminals = self.terminals.clone();
                let session = tokio::task::spawn_blocking(move || {
                    terminals.open(params.chat_id, &cwd, params.cols, params.rows)
                })
                .await
                .map_err(|e| RpcError::Failed(e.to_string()))??;
                RpcReply::value(&session)
            }
            methods::SUBSCRIBE_TERMINAL => {
                let params: holt_rpc::terminals::SubscribeTerminal = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                self.terminals
                    .subscribe(&params.terminal_id, params.after_seq)
            }
            methods::WRITE_TERMINAL => {
                let params: holt_rpc::terminals::WriteTerminal = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                let terminals = self.terminals.clone();
                tokio::task::spawn_blocking(move || {
                    terminals.write(&params.terminal_id, &params.data)
                })
                .await
                .map_err(|e| RpcError::Failed(e.to_string()))??;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::RESIZE_TERMINAL => {
                let params: holt_rpc::terminals::ResizeTerminal = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                self.terminals
                    .resize(&params.terminal_id, params.cols, params.rows)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::LIST_TERMINALS => RpcReply::value(&self.terminals.list()),
            methods::CLOSE_TERMINAL | methods::CLOSE_ALL_TERMINALS => {
                let id = if method == methods::CLOSE_TERMINAL {
                    Some(
                        serde_json::from_value::<holt_rpc::terminals::TerminalId>(params)
                            .map_err(|error| RpcError::BadParams(error.to_string()))?
                            .terminal_id,
                    )
                } else {
                    None
                };
                let terminals = self.terminals.clone();
                tokio::task::spawn_blocking(move || match id {
                    Some(id) => terminals.close(&id),
                    None => terminals.close_all(false),
                })
                .await
                .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::WATCH_MESSAGE_QUEUE => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let receiver = chat
                    .queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .tx
                    .subscribe();
                Ok(Self::watch_value(receiver))
            }
            methods::WATCH_CHAT_USAGE => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                // The denominator's inputs are seeded here: opening the watch
                // is the one moment the engine holds both the model catalog
                // and the chat. From then on the frame reads the live queue
                // itself, so a queued run on another model moves the window
                // without any further bookkeeping.
                crate::usage::seed_occupancy(
                    &chat,
                    self.providers.context_windows().into_iter().collect(),
                    self.selected_wire_model(chat_id),
                );
                // Subscribe first, then publish: the new receiver's opening
                // value is the frame seeded just above, and any other
                // subscriber on this chat simply gets the refresh too.
                let receiver = chat.usage_tx.subscribe();
                crate::usage::publish(&chat);
                Ok(Self::watch_value(receiver))
            }
            methods::USAGE_STATS => {
                let params: UsageStatsParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if !matches!(params.days, 7 | 30 | 90) {
                    return Err(RpcError::BadParams(format!(
                        "days must be 7, 30, or 90, got {}",
                        params.days
                    )));
                }
                // The aggregate walks every ledger on disk; like the other
                // blocking-FS reads, that runs off the async workers.
                let data_dir = self.data_dir.clone();
                let reply = tokio::task::spawn_blocking(move || {
                    crate::usage_stats::stats(&data_dir, params.days)
                })
                .await
                .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&reply)
            }
            methods::WATCH_TURN_TERMINAL_EVENTS => {
                let stream = futures::stream::unfold(
                    self.turn_events.subscribe(),
                    |mut receiver| async move {
                        loop {
                            match receiver.recv().await {
                                Ok(event) => match serde_json::to_value(&event) {
                                    Ok(value) => return Some((value, receiver)),
                                    Err(_) => continue,
                                },
                                // A lagging subscriber drops what it missed —
                                // a consumer failure, never a Turn failure —
                                // and keeps receiving new events.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    return None;
                                }
                            }
                        }
                    },
                );
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            methods::WATCH_TURN_RETRY => {
                let stream = futures::stream::unfold(
                    self.runtime.retry_events.subscribe(),
                    |mut receiver| async move {
                        loop {
                            match receiver.recv().await {
                                Ok(notice) => match serde_json::to_value(&notice) {
                                    Ok(value) => return Some((value, receiver)),
                                    Err(_) => continue,
                                },
                                // A lagging subscriber drops what it missed —
                                // a consumer failure, never a run failure —
                                // and keeps receiving new notices.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    return None;
                                }
                            }
                        }
                    },
                );
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            methods::CONTINUE_MESSAGE_QUEUE => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let snapshot = {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    // Continue lifts the single-run scope of an attended
                    // send along with the pause (ADR-0021); a later failure
                    // pauses again.
                    queue.resume()?;
                    queue.snapshot()
                };
                self.kick_queue(chat);
                RpcReply::value(&snapshot)
            }
            methods::EDIT_QUEUED_MESSAGE => {
                let chat_id = required_string(&params, "chatId")?;
                let message_id = required_string(&params, "messageId")?;
                let prompt = required_string(&params, "prompt")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let snapshot = {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    queue.edit(message_id, prompt.to_string())?;
                    queue.snapshot()
                };
                RpcReply::value(&snapshot)
            }
            methods::EDIT_LAST_MESSAGE => self.edit_last_message(params).await,
            methods::DELETE_QUEUED_MESSAGE => {
                let chat_id = required_string(&params, "chatId")?;
                let message_id = required_string(&params, "messageId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let snapshot = {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    queue.delete(message_id)?;
                    queue.snapshot()
                };
                RpcReply::value(&snapshot)
            }
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => RpcReply::value(&serde_json::json!({ "ready": true })),
            methods::LOCAL_DEVICE => RpcReply::value(&serde_json::json!({
                "deviceId": self.engine_info.device_id,
            })),
            methods::LIST_PROVIDERS => RpcReply::value(&self.providers.providers().await),
            methods::SAVE_PROVIDER_KEY => {
                let provider = required_string(&params, "providerId")?;
                let key = required_string(&params, "key")?;
                if !self.providers.is_eligible(provider) {
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
            methods::LIST_MODELS => {
                let provider = required_string(&params, "providerId")?;
                RpcReply::value(&self.providers.models_for(provider))
            }
            methods::LIST_HIDDEN_MODELS => {
                let provider = required_string(&params, "providerId")?;
                let rows: Vec<serde_json::Value> = self
                    .providers
                    .settings
                    .hidden_models_for(provider)
                    .iter()
                    .map(|model_id| {
                        let label = self
                            .providers
                            .resolve_model(provider, &format!("{provider}/{model_id}"))
                            .ok()
                            .map(|model| model.name);
                        serde_json::json!({ "id": format!("{provider}/{model_id}"), "label": label })
                    })
                    .collect();
                RpcReply::value(&rows)
            }
            // The Settings review panel (model setup v2): the write path the
            // setup agent never holds. Revalidation rides the same
            // `apply_changes` the old tool used; the button is the human
            // approval.
            methods::APPLY_MODEL_PROPOSAL => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let proposal_id = required_string(&params, "proposalId")?;
                let chat = self.runtime.chat(chat_id);
                if chat.is_removed() {
                    return Err(RpcError::Failed("chat was deleted".into()));
                }
                let applied =
                    crate::tools::model_setup::apply_stored(&self.providers, &chat, proposal_id)
                        .map_err(RpcError::Failed)?;
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({ "applied": applied }))
            }
            methods::LIST_MODEL_PROPOSALS => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                RpcReply::value(&crate::tools::model_setup::proposal_views(&chat))
            }
            methods::DISCARD_MODEL_PROPOSAL => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let proposal_id = required_string(&params, "proposalId")?;
                let chat = self.runtime.chat(chat_id);
                let discarded = crate::tools::model_setup::discard_stored(&chat, proposal_id);
                RpcReply::value(&serde_json::json!({ "discarded": discarded }))
            }
            methods::START_MODEL_SETUP_CHAT => self.start_model_setup_chat(params),
            // The Key request's read half (ADR-0031): the dialog's card.
            methods::GET_PROVIDER_KEY_REQUEST => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let view =
                    crate::tools::model_setup::key_request_view(&self.providers, &chat).await;
                // `{}` when none: a bare JSON null on the wire reads back
                // as an absent `ok` field and would hang the client call.
                RpcReply::value(&view.unwrap_or(serde_json::json!({})))
            }
            // The settle (ADR-0031): one engine-owned operation — save the
            // key (never the chat), queue the fixed notice, clear the card.
            methods::SETTLE_PROVIDER_KEY_REQUEST => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                if chat.is_removed() {
                    return Err(RpcError::Failed("chat was deleted".into()));
                }
                // Take (not clone) the request: a second concurrent settle
                // finds none, and any failure below puts it back so the
                // card stays and a retry is idempotent.
                let pending = chat
                    .key_request
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .ok_or_else(|| {
                        RpcError::Failed("no pending key request on this chat".into())
                    })?;
                let restore = || {
                    *chat.key_request.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(pending.clone());
                    chat.save_provider_mode();
                };
                let saved = if let Some(key) = params
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                {
                    if let Err(error) = self
                        .providers
                        .credentials
                        .save_key(&pending.provider_id, key)
                        .await
                    {
                        restore();
                        return Err(RpcError::Failed(error.to_string()));
                    }
                    // The entry approves the destination the card showed
                    // (ADR-0031): this chat's planned-target probes may
                    // now carry the key against the exact same baseUrl.
                    chat.approved_key_destinations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert((pending.provider_id.clone(), pending.destination.clone()));
                    chat.save_provider_mode();
                    true
                } else if params.get("key").is_some() {
                    restore();
                    return Err(RpcError::BadParams("key must not be empty".into()));
                } else {
                    false
                };
                let notice = if saved {
                    crate::tools::model_setup::key_saved_notice(
                        &pending.provider_id,
                        &pending.destination,
                    )
                } else {
                    crate::tools::model_setup::key_dismissed_notice(&pending.provider_id)
                };
                // The notice rides the queue as an ordinary user message on
                // the setup chat's own model — exactly what the composer
                // would have sent had the user typed it.
                let (config, cwd) = {
                    let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
                    let row = chats
                        .iter()
                        .find(|row| row.id == chat_id)
                        .ok_or_else(|| RpcError::Failed("chat was deleted".into()))?;
                    let cwd = row.cwd.clone().unwrap_or_else(|| {
                        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
                    });
                    (row.config.clone(), cwd)
                };
                let config = config
                    .ok_or_else(|| RpcError::Failed("the setup chat has no model config".into()))?;
                let message_id = format!("key-request-{}", uuid::Uuid::new_v4());
                if let Err(error) = self.enqueue_run(
                    chat.clone(),
                    Self::queued_run_request(&config, &notice, cwd),
                    message_id,
                ) {
                    restore();
                    return Err(error);
                }
                chat.save_provider_mode();
                RpcReply::value(&serde_json::json!({
                    "settled": if saved { "saved" } else { "dismissed" },
                    "providerId": pending.provider_id,
                    "destination": pending.destination,
                }))
            }
            methods::LIST_API_DIALECTS => {
                let ids: Vec<String> = pi_core::ai::compat::get_api_providers()
                    .iter()
                    .map(|provider| provider.api.clone())
                    .collect();
                RpcReply::value(&serde_json::json!(ids))
            }
            methods::SAVE_CUSTOM_PROVIDER => {
                let id = required_string(&params, "id")?.trim().to_string();
                let name = required_string(&params, "name")?.trim().to_string();
                let base_url = required_string(&params, "baseUrl")?.trim().to_string();
                let default_api = required_string(&params, "defaultApi")?.trim().to_string();
                let provider = crate::provider_settings::CustomProvider {
                    id,
                    name,
                    base_url,
                    default_api,
                };
                self.providers
                    .validate_custom_provider(&provider)
                    .map_err(RpcError::BadParams)?;
                self.providers
                    .settings
                    .upsert_custom_provider(provider)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REMOVE_CUSTOM_PROVIDER => {
                let provider = required_string(&params, "providerId")?;
                self.providers
                    .settings
                    .remove_custom_provider(provider)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SAVE_MODEL_RECORD => {
                let provider = required_string(&params, "providerId")?;
                let mut record = params
                    .get("record")
                    .cloned()
                    .ok_or_else(|| RpcError::BadParams("record is required".into()))?;
                // A record may omit `baseUrl`: adding a model to a provider
                // means riding the provider's endpoint, so the default fills
                // it in. An explicit value — the endpoint-fix case — wins.
                let carries_base_url = record
                    .get("baseUrl")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|url| !url.trim().is_empty());
                if !carries_base_url {
                    let base_url = self.providers.default_base_url(provider).ok_or_else(|| {
                        RpcError::BadParams(
                            "record needs a baseUrl: the provider declares no default \
                                 endpoint"
                                .into(),
                        )
                    })?;
                    if let Some(slot) = record.as_object_mut() {
                        slot.insert("baseUrl".to_string(), serde_json::Value::String(base_url));
                    }
                }
                let record: CoreModel = serde_json::from_value(record)
                    .map_err(|_| RpcError::BadParams("record must be a model record".into()))?;
                self.providers
                    .validate_model_record(provider, &record)
                    .map_err(RpcError::BadParams)?;
                self.providers
                    .settings
                    .upsert_model_record(provider, record)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::REMOVE_MODEL_RECORD => {
                let provider = required_string(&params, "providerId")?;
                let submitted = required_string(&params, "modelId")?.trim();
                let qualified_prefix = format!("{provider}/");
                let model = submitted
                    .strip_prefix(&qualified_prefix)
                    .unwrap_or(submitted);
                let removed_record = self
                    .providers
                    .settings
                    .remove_model_record(provider, model)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                if !removed_record {
                    self.providers
                        .settings
                        .remove_custom_model(provider, model)
                        .map_err(|error| RpcError::Failed(error.to_string()))?;
                }
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SET_HIDDEN_MODELS => {
                let provider = required_string(&params, "providerId")?;
                let submitted = required_string_list(&params, "modelIds")?;
                let qualified_prefix = format!("{provider}/");
                let mut model_ids = std::collections::BTreeSet::new();
                for id in submitted.iter().map(String::as_str) {
                    let id = id.trim();
                    let model = id.strip_prefix(&qualified_prefix).unwrap_or(id);
                    if !self.providers.has_model(provider, model) {
                        return Err(RpcError::BadParams(format!(
                            "unknown model for {provider}: {model}"
                        )));
                    }
                    model_ids.insert(model.to_string());
                }
                self.providers
                    .settings
                    .set_hidden_models(provider, model_ids)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::RESET_PROVIDER_CATALOG => {
                match optional_string(&params, "providerId") {
                    Some(provider) => {
                        self.providers
                            .settings
                            .reset_provider(&provider)
                            .map_err(|error| RpcError::Failed(error.to_string()))?;
                    }
                    // Global reset: back to the compiled catalog under the
                    // hand-edited overlay for every provider (ADR-0028).
                    None => {
                        self.providers
                            .settings
                            .reset_all()
                            .map_err(|error| RpcError::Failed(error.to_string()))?;
                    }
                }
                self.refresh_catalog_windows();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::GET_TITLE_SETTINGS => RpcReply::value(&self.title_settings_state().await),
            methods::SAVE_TITLE_SETTINGS => self.save_title_settings(params).await,
            methods::GET_WEB_SEARCH_SETTINGS => RpcReply::value(&self.web_search_state()),
            methods::SAVE_WEB_SEARCH_SETTINGS => self.save_web_search_settings(params).await,
            methods::REVEAL_WEB_SEARCH_KEY => RpcReply::value(&serde_json::json!({
                "key": self.web_search.get().map(|record| record.api_key),
            })),
            methods::REMOVE_WEB_SEARCH_SETTINGS => {
                self.web_search
                    .remove()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::GET_JEV_SETTINGS => RpcReply::value(&self.jev_state()),
            methods::SAVE_JEV_SETTINGS => self.save_jev_settings(params).await,
            methods::REVEAL_JEV_KEY => RpcReply::value(&serde_json::json!({
                "key": self.jev.get().map(|record| record.api_key),
            })),
            methods::REMOVE_JEV_SETTINGS => {
                self.jev
                    .remove()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
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
            // The composer's slash menu (ADR-0011/0025): the commands this
            // backend intercepts itself.
            methods::LIST_COMMANDS => RpcReply::value(&serde_json::json!([
                { "name": "compact", "description": "Summarize the older conversation and keep only a recent tail" },
                { "name": "init", "description": "Generate or update AGENTS.md for this repository" },
                { "name": "plan", "description": "Plan Mode: explore read-only, submit a plan for approval", "inputHint": "[task]" }
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

            // Fuzzy path search for the composer's `@`-mention palette: the
            // root comes from chat/space state, the walk+match runs off the
            // async workers.
            methods::SEARCH_FILES => {
                let params: SearchFilesParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&params)?;
                let query = params.query.clone();
                let matches = tokio::task::spawn_blocking(move || {
                    crate::path_search::search(std::path::Path::new(&root), &query)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("search task failed: {error}")))?;
                RpcReply::value(&matches)
            }

            // File sidebar (ADR-0020 groundwork): one directory level and
            // bounded text reads, fenced behind the owning space's root. The
            // blocking FS work runs off the async workers like SearchFiles.
            methods::LIST_WORKSPACE_ENTRIES => {
                let params: WorkspacePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                params.check_selector()?;
                let root = self.search_files_root(&params.as_search_root())?;
                let requested = params.path.clone().unwrap_or_default();
                let listing = tokio::task::spawn_blocking(move || {
                    crate::files::list_directory(std::path::Path::new(&root), &requested)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("listing task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::READ_WORKSPACE_FILE => {
                let params: WorkspacePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                params.check_selector()?;
                let path = params.require_path()?;
                let root = self.search_files_root(&params.as_search_root())?;
                // Skill roots ride along: a personal/holt `SKILL.md` opens
                // in a sidebar file tab via its absolute catalog path.
                let skill_roots = self.skills.out_of_workspace_roots();
                let read = tokio::task::spawn_blocking(move || {
                    crate::files::read_file(std::path::Path::new(&root), &path, &skill_roots)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("read task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&read)
            }
            methods::READ_WORKSPACE_IMAGE => {
                let params: WorkspacePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                params.check_selector()?;
                let path = params.require_path()?;
                let root = self.search_files_root(&params.as_search_root())?;
                // The fence (root containment, `.git`, symlink landing paths)
                // runs before any bytes move; the bounded sniffed read itself
                // is the images store's, under the same limits as ReadImage.
                let canonical = tokio::task::spawn_blocking(move || {
                    crate::files::resolve_image_target(std::path::Path::new(&root), &path)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("image read task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                let display = canonical.display().to_string();
                let (mime_type, data) =
                    self.images.read(&display).await.map_err(RpcError::Failed)?;
                RpcReply::value(&holt_rpc::images::WorkspaceImageData {
                    path: display,
                    mime_type,
                    data,
                })
            }
            methods::SAVE_WORKSPACE_FILE => {
                let params: SaveWorkspaceFileParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let (path, text, version, expect_disk_version, bom) = (
                    params.path,
                    params.text,
                    params.version,
                    params.expect_disk_version,
                    params.bom,
                );
                let save = tokio::task::spawn_blocking(move || {
                    crate::files::save_file(
                        std::path::Path::new(&root),
                        &path,
                        &text,
                        &version,
                        expect_disk_version.as_deref(),
                        bom,
                    )
                })
                .await
                .map_err(|error| RpcError::Failed(format!("save task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&save)
            }
            methods::WRITE_WORKSPACE_FILE_AS => {
                let params: WriteFileAsParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let (path, text, bom) = (params.path, params.text, params.bom);
                let saved = tokio::task::spawn_blocking(move || {
                    crate::files::write_file_as(std::path::Path::new(&root), &path, &text, bom)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("save-as task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&saved)
            }
            methods::WATCH_WORKSPACE_ENTRIES => {
                let params: WorkspacePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                params.check_selector()?;
                let root = self.search_files_root(&params.as_search_root())?;
                let canonical = std::path::Path::new(&root)
                    .canonicalize()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                let stream =
                    crate::workspace_watch::subscribe(canonical).map_err(RpcError::Failed)?;
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            // Working-tree Git status decorations (file-sidebar ticket 10):
            // a focused extension of the git watch — a fresh snapshot after
            // every change under the selector's space root (working tree or
            // `.git`), keyed to that Space's folder, never a diff scope.
            methods::WATCH_WORKSPACE_GIT_STATUS => {
                let params: WorkspacePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                params.check_selector()?;
                let root = self.search_files_root(&params.as_search_root())?;
                let canonical = std::path::Path::new(&root)
                    .canonicalize()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                let stream = crate::git_status_watch::subscribe(canonical, self.git.clone())
                    .map_err(RpcError::Failed)?;
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            methods::CREATE_WORKSPACE_ENTRY => {
                let params: CreateEntryParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let (parent, name, is_dir) = (
                    params.parent_path.unwrap_or_default(),
                    params.name,
                    params.is_dir,
                );
                tokio::task::spawn_blocking(move || {
                    crate::files::create_entry(std::path::Path::new(&root), &parent, &name, is_dir)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("create task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::RENAME_WORKSPACE_ENTRY => {
                let params: RenameEntryParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let (path, new_name) = (params.path, params.new_name);
                let destination = tokio::task::spawn_blocking(move || {
                    crate::files::rename_entry(std::path::Path::new(&root), &path, &new_name)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("rename task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": destination }))
            }
            methods::MOVE_WORKSPACE_ENTRY => {
                let params: MoveEntryParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let (path, destination_directory) = (params.path, params.destination_directory);
                let destination = tokio::task::spawn_blocking(move || {
                    crate::files::move_entry(
                        std::path::Path::new(&root),
                        &path,
                        &destination_directory,
                    )
                })
                .await
                .map_err(|error| RpcError::Failed(format!("move task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": destination }))
            }
            methods::TRASH_WORKSPACE_ENTRY => {
                let params: TrashEntryParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                if params.chat_id.is_some() == params.space_id.is_some() {
                    return Err(RpcError::BadParams(
                        "exactly one of chatId or spaceId is required".into(),
                    ));
                }
                let root = self.search_files_root(&SearchFilesParams {
                    query: String::new(),
                    chat_id: params.chat_id,
                    space_id: params.space_id,
                })?;
                let path = params.path;
                tokio::task::spawn_blocking(move || {
                    crate::files::trash_entry(std::path::Path::new(&root), &path)
                })
                .await
                .map_err(|error| RpcError::Failed(format!("trash task failed: {error}")))?
                .map_err(|fault| RpcError::Failed(fault.to_string()))?;
                RpcReply::value(&serde_json::json!({}))
            }

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
            methods::CREATE_BRANCH => {
                let repo_path = required_string(&params, "repoPath")?;
                let name = required_string(&params, "name")?;
                let base_ref = params
                    .get("baseRef")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                self.git
                    .create_branch(repo_path, name, base_ref)
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
            methods::FETCH_TOOL_BLOB => {
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
                let text = serde_json::to_string(&*entries)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({"text": text}))
            }
            methods::WATCH_CHECKOUT_CHANGE_REQUEST => Ok(pending_stream()),

            // Live checkout-diff awareness (git-capability issue 03): the
            // hub owns one watcher per git space; first subscriber starts
            // it, last stops it.
            methods::WATCH_CHECKOUT_DIFFS => Ok(self.watch.subscribe()),

            // The diff family. Working-tree mode is the live capture;
            // branch mode diffs the merge-base with a chosen base ref;
            // commit mode pins parent → commit without the working tree.
            // The turn mode arrives with its slice.
            methods::GET_CHECKOUT_DIFF => {
                let cwd = required_string(&params, "cwd")?;
                let mode = required_string(&params, "mode")?;
                let base_ref = params
                    .get("baseRef")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let commit_sha = params
                    .get("commitSha")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                match mode {
                    "commit" => {
                        let commit_sha = commit_sha
                            .filter(|sha| !sha.trim().is_empty())
                            .ok_or_else(|| {
                                RpcError::BadParams("commitSha is required for commit diffs".into())
                            })?;
                        let diff = self
                            .git
                            .commit_diff(cwd, &self.engine_info.device_id, &commit_sha)
                            .await
                            .map_err(git_fault)?;
                        RpcReply::value(&diff)
                    }
                    "turn" => {
                        let chat_id = required_string(&params, "chatId")?;
                        // No snapshot (never ran, engine restarted) is an
                        // explicit error — never a silent empty diff.
                        let Some(record) = self.turn_changes.snapshot(chat_id) else {
                            return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                        };
                        let diff = self
                            .git
                            .turn_diff(
                                cwd,
                                &self.engine_info.device_id,
                                &record.baseline,
                                Some(&record.attribution.snapshot()),
                            )
                            .await
                            .map_err(git_fault)?;
                        RpcReply::value(&diff)
                    }
                    "workingTree" | "branch" => {
                        let diff = self
                            .git
                            .capture(cwd, &self.engine_info.device_id, mode, base_ref.as_deref())
                            .await
                            .map_err(git_fault)?;
                        RpcReply::value(&diff)
                    }
                    other => Err(RpcError::Failed(format!(
                        "{other} diffs are not available yet"
                    ))),
                }
            }
            methods::GET_CHECKOUT_FILE_DIFF_TEXT => {
                let request: holt_proto::GetCheckoutFileDiffTextRequest =
                    holt_rpc::parse_params(params)?;
                match request.mode.as_str() {
                    "turn" => {
                        let Some(chat_id) = request.chat_id.as_deref().filter(|id| !id.is_empty())
                        else {
                            return Err(RpcError::BadParams(
                                "chatId is required for turn diffs".into(),
                            ));
                        };
                        // A Turn-addressed read (ADR-0024 ticket 02) serves
                        // the settled Turn's immutable before/after pair
                        // from its persisted record: restarts and later
                        // workspace edits cannot move history. Without a
                        // record — the Turn still runs, or its write failed —
                        // only the chat's CURRENT Turn may fall through to
                        // the live baseline; an older Turn has no reviewable
                        // pair to invent.
                        let addressed = request
                            .message_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|id| !id.is_empty());
                        if let Some(message_id) = addressed {
                            if let Some(record) =
                                crate::turn_change_store::load(&self.data_dir, chat_id, message_id)
                            {
                                let Some(content) = record.content_for(&request.path) else {
                                    return Err(RpcError::Failed(format!(
                                        "{} is not part of that turn's changes",
                                        request.path
                                    )));
                                };
                                return RpcReply::value(&holt_proto::CheckoutFileDiffText {
                                    diff_checksum: request.diff_checksum.clone(),
                                    old_text: content.old_text.clone(),
                                    new_text: content.new_text.clone(),
                                    old_content_hash: content.old_content_hash.clone(),
                                    new_content_hash: content.new_content_hash.clone(),
                                    binary: content.binary,
                                    truncated: content.truncated,
                                    stale: false,
                                });
                            }
                            let is_current = self
                                .turn_changes
                                .snapshot(chat_id)
                                .is_some_and(|record| record.message_id == message_id);
                            if !is_current {
                                return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                            }
                        }
                        let Some(record) = self.turn_changes.snapshot(chat_id) else {
                            return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                        };
                        let text = self
                            .git
                            .turn_file_text(
                                &request.cwd,
                                &self.engine_info.device_id,
                                &request,
                                &record.baseline,
                                Some(&record.attribution.snapshot()),
                            )
                            .await
                            .map_err(git_fault)?;
                        RpcReply::value(&text)
                    }
                    "" | "workingTree" | "branch" | "commit" => {
                        let text = self
                            .git
                            .capture_file_text(&request.cwd, &self.engine_info.device_id, &request)
                            .await
                            .map_err(git_fault)?;
                        RpcReply::value(&text)
                    }
                    other => Err(RpcError::Failed(format!(
                        "{other} diffs are not available yet"
                    ))),
                }
            }

            // Turn change sets (ADR-0024): the net Git change from a Turn's
            // admission baseline to its live or final working tree. Separate
            // from the checkout-diff scopes, which the UI's Changes pane
            // owns; this family feeds the Turn card. With `messageId` the
            // read addresses one specific Turn — the in-memory record while
            // the engine knows it, else the persisted record a restart
            // restores (ticket 02).
            methods::GET_TURN_CHANGE_SET => {
                let chat_id = required_string(&params, "chatId")?;
                let root = self.turn_change_root(chat_id)?;
                let message_id = optional_string(&params, "messageId");
                let Some(message_id) = message_id else {
                    // The chat's current Turn is a live Git read: the non-Git
                    // answer keys on the working directory, never on a
                    // capture error.
                    if !self.git.is_work_tree(&root).await {
                        return RpcReply::value(&TurnChangeSetReply::Unsupported {
                            reason: NON_GIT_CHANGE_SET_REASON.into(),
                        });
                    }
                    return match self
                        .turn_changes
                        .read(&self.git, &self.engine_info.device_id, chat_id)
                        .await
                        .map_err(git_fault)?
                    {
                        Some(change_set) => {
                            RpcReply::value(&TurnChangeSetReply::Captured(change_set))
                        }
                        None => Err(RpcError::Failed(NO_TURN_RECORDED.into())),
                    };
                };
                // Memory first (a live Turn, or the frozen current/last
                // one); a restart — or a working tree that can no longer be
                // captured — falls back to the persisted record, which
                // outlives the repository it came from: only a Turn with no
                // record anywhere answers by its working directory.
                let memory = self
                    .turn_changes
                    .read_message(&self.git, &self.engine_info.device_id, chat_id, &message_id)
                    .await;
                if let Ok(Some(change_set)) = memory {
                    return RpcReply::value(&TurnChangeSetReply::Captured(change_set));
                }
                if let Some(record) =
                    crate::turn_change_store::load(&self.data_dir, chat_id, &message_id)
                {
                    return RpcReply::value(&TurnChangeSetReply::Captured(
                        record.change_set(chat_id),
                    ));
                }
                if !self.git.is_work_tree(&root).await {
                    return RpcReply::value(&TurnChangeSetReply::Unsupported {
                        reason: NON_GIT_CHANGE_SET_REASON.into(),
                    });
                }
                match memory {
                    Err(fault) => Err(git_fault(fault)),
                    Ok(_) => Err(RpcError::Failed(NO_TURN_RECORDED.into())),
                }
            }
            methods::WATCH_TURN_CHANGE_SET => {
                let chat_id = required_string(&params, "chatId")?.to_string();
                let root = self.turn_change_root(&chat_id)?;
                if !self.git.is_work_tree(&root).await {
                    use futures::StreamExt;
                    let value = serde_json::to_value(TurnChangeSetReply::Unsupported {
                        reason: NON_GIT_CHANGE_SET_REASON.into(),
                    })
                    .map_err(|error| RpcError::Failed(format!("serialize response: {error}")))?;
                    return Ok(RpcReply::Stream(futures::stream::iter([value]).boxed()));
                }
                let stream = crate::turn_change_watch::subscribe(
                    std::path::PathBuf::from(root),
                    chat_id,
                    self.git.clone(),
                    self.engine_info.device_id.clone(),
                    self.turn_changes.clone(),
                    self.turn_events.clone(),
                )
                .map_err(RpcError::Failed)?;
                Ok(RpcReply::Stream(Box::pin(stream)))
            }

            // History: the topologically ordered commit graph with refs,
            // paged by cursor, plus the fetch action that updates
            // remote-tracking refs without touching any checkout state.
            methods::LIST_GIT_HISTORY => {
                let cwd = required_string(&params, "cwd")?;
                let cursor = params
                    .get("cursor")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                let limit = params
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(50)
                    .clamp(1, 500) as usize;
                let page = self
                    .git
                    .history(cwd, cursor, limit)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&page)
            }
            methods::FETCH_ALL => {
                let repo_path = required_string(&params, "repoPath")?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    self.git.fetch_all(repo_path),
                )
                .await
                .map_err(|_| RpcError::Failed("fetch timed out after 30s".into()))?
                .map_err(RpcError::Failed)?;
                RpcReply::value(&serde_json::json!({}))
            }
            // The Git panel's write trio (ADR-0022): the engine's first
            // content-mutating git operations, served for the UI only —
            // the agent tool surface stays read-only. The safety gates
            // (path validation, conflict and in-progress-operation
            // refusals) live engine-side in `git.rs`.
            methods::STAGE_PATHS => {
                let repo_path = required_string(&params, "repoPath")?;
                let paths = required_string_list(&params, "paths")?;
                self.git
                    .stage_paths(repo_path, paths)
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::UNSTAGE_PATHS => {
                let repo_path = required_string(&params, "repoPath")?;
                let paths = required_string_list(&params, "paths")?;
                self.git
                    .unstage_paths(repo_path, paths)
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&serde_json::json!({}))
            }
            methods::COMMIT_STAGED => {
                let repo_path = required_string(&params, "repoPath")?;
                let message = required_string(&params, "message")?;
                let sha = self
                    .git
                    .commit_staged(repo_path, message)
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&serde_json::json!({ "sha": sha }))
            }

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
            methods::ENTER_PROVIDER_MODE => self.enter_provider_mode(params),
            methods::EXIT_PROVIDER_MODE => self.exit_provider_mode(params),
            methods::GET_PROVIDER_MODE => {
                RpcReply::value(&self.provider_mode_state(required_string(&params, "chatId")?)?)
            }
            methods::GET_PLAN_MODE => {
                RpcReply::value(&self.plan_mode_state(required_string(&params, "chatId")?)?)
            }
            methods::RESOLVE_PLAN_APPROVAL => self.resolve_plan_approval(params),

            // Local image surface (engine/src/images.rs): bounded preview
            // reads, pasted-image staging, and draft-chip release.
            methods::READ_IMAGE => {
                let params: holt_rpc::images::ImagePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                let path = &params.path;
                let (mime_type, data) = self.images.read(path).await.map_err(RpcError::Failed)?;
                RpcReply::value(&holt_rpc::images::ImageData { mime_type, data })
            }
            methods::STAGE_IMAGE => {
                let params: holt_rpc::images::StageImageParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                let data = &params.data;
                if data.len() > (crate::images::MAX_IMAGE_BYTES as usize).div_ceil(3) * 4 {
                    return Err(RpcError::BadParams(
                        "Image exceeds the 25 MiB limit.".into(),
                    ));
                }
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data.as_bytes(),
                )
                .map_err(|error| RpcError::BadParams(format!("data is not base64: {error}")))?;
                let staged = self.images.stage(bytes).await.map_err(RpcError::Failed)?;
                RpcReply::value(&staged)
            }
            methods::RELEASE_IMAGE => {
                let params: holt_rpc::images::ImagePathParams = serde_json::from_value(params)
                    .map_err(|error| RpcError::BadParams(error.to_string()))?;
                let path = &params.path;
                let released = self.images.release(path).await.map_err(RpcError::Failed)?;
                RpcReply::value(&holt_rpc::images::ReleaseImageResult { released })
            }

            // Everything this backend has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

/// The chat-less terminal root (`OpenTerminal`): the caller's explicit cwd,
/// else the user's home directory. An empty override means "not given".
fn chatless_terminal_root(cwd: Option<String>) -> Result<String, RpcError> {
    match cwd.filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => Ok(cwd),
        None => crate::local_fs::home_dir()
            .ok_or_else(|| RpcError::Failed("could not resolve your home folder".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::chatless_terminal_root;

    #[test]
    fn chatless_terminal_root_prefers_the_explicit_cwd() {
        assert_eq!(
            chatless_terminal_root(Some("/tmp/holt-root".into())).unwrap(),
            "/tmp/holt-root"
        );
        let home = chatless_terminal_root(None).unwrap();
        assert!(!home.is_empty());
        assert_eq!(chatless_terminal_root(Some(String::new())).unwrap(), home);
    }
}
