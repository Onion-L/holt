//! The RPC surface: `RpcService` dispatch plus the space/chat mutation and
//! queue-command handlers it routes to.

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry, TranscriptFrame,
    diff_transcript,
};
use holt_proto::{
    AuthState, Chat, ChatConfig, PendingKind, RunRequest, SessionStatus, Space, TitleSettings,
    TitleSettingsState, TitleSource,
};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentRun, ChatRuntime};
use crate::local_fs::{list_drives, list_folders, local_device};
use crate::providers::ProviderAdapter;
use crate::store::{persist_chats, persist_spaces};
use crate::title_settings::MAX_TITLE_INSTRUCTION_CHARS;
use crate::{EngineService, LocalEngine};

/// The sidebar title ceiling shared by the first-line fallback, manual
/// renames, and automatic titles.
pub(crate) const TITLE_CHAR_LIMIT: usize = 60;

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
        persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
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
        if !crate::store::chat_id_is_path_safe(&params.chat_id) {
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
                    queue.enqueue(request, message_id, PendingKind::Ordinary, None, None)?;
                }
                self.kick_queue(chat);
            }
            SessionCommandPayload::InvokeSkill {
                request,
                name,
                extra_instructions,
                message_id,
            } => {
                if message_id.trim().is_empty() || name.trim().is_empty() {
                    return Err(RpcError::BadParams(
                        "messageId and skill name must not be empty".into(),
                    ));
                }
                // Typed from submission to execution: the skill itself is
                // resolved against a fresh catalog when the queue admits the
                // item (rule 16) — an unknown or invalid name then retains
                // the pending item with an error instead of failing here.
                {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    queue.enqueue(
                        request,
                        message_id,
                        PendingKind::Skill,
                        Some(name),
                        extra_instructions,
                    )?;
                }
                self.kick_queue(chat);
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
                    queue.enqueue(request, message_id, PendingKind::Compact, None, None)?;
                }
                self.kick_queue(chat);
            }
        }
        RpcReply::value(&serde_json::json!({}))
    }

    /// Accept and launch one queued Turn — the driver's tail for ordinary
    /// messages and skill invocations. `parts` is the transcript user entry
    /// (prompt text or skill chip), `preview` the sidebar/title text,
    /// `prompt` the model-visible text, and `invocation` the chip seeded at
    /// the head of the run's own entry (skill invocations only). For a
    /// queued skill the caller resolves the skill against a fresh catalog
    /// and passes it as `resolved_skill`; the admission checkpoint then
    /// rebuilds all of the above from the item the queue holds NOW, so an
    /// edit of the extra instructions that landed mid-pick wins.
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
        mut invocation: Option<MessagePart>,
        mut title_prompt: Option<String>,
        cancel: CancellationToken,
        queued: bool,
        resolved_skill: Option<pi_core::agent::harness::types::Skill>,
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
        if queued {
            let admitted = {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() || chat.is_removed() {
                    return Err(RpcError::Failed("Turn interrupted before execution".into()));
                }
                queue.start(&message_id, timestamp)?
            };
            match admitted.message.kind {
                PendingKind::Ordinary => {
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
                }
                PendingKind::Skill => {
                    let skill = resolved_skill
                        .as_ref()
                        .expect("a queued skill Turn resolves its skill at admission");
                    let extra = admitted
                        .message
                        .extra_instructions
                        .filter(|extra| !extra.trim().is_empty());
                    let block = crate::skills::invocation_prompt(skill, None);
                    prompt = crate::skills::invocation_prompt(skill, extra.as_deref());
                    preview = format!(
                        "/skill {}",
                        admitted
                            .message
                            .skill_name
                            .as_deref()
                            .unwrap_or(&skill.name)
                    );
                    // The user entry keeps the compact chip (name + source
                    // pointer); the `<skill>` block rides the AGENT entry's
                    // opening chip instead — the reply opens with what the
                    // model was told to follow, ahead of any thinking. The
                    // raw `/skill` directive never appears anywhere.
                    parts = crate::skills::user_entry_parts(
                        skill.name.clone(),
                        skill.file_path.clone(),
                        extra,
                    );
                    invocation = Some(MessagePart::Skill {
                        id: "s0".into(),
                        name: skill.name.clone(),
                        file: skill.file_path.clone(),
                        content: Some(block),
                    });
                    title_prompt = None;
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
            self.prepare_title_task(chat_id, title_prompt.as_deref().unwrap_or_default())
                .await
        } else {
            None
        };

        let persistence = self
            .runtime
            .persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut title_spawn = None;
        // The Turn's mode snapshot (ADR-0014): the stored mode, or the
        // sticky default for a row without a config yet. Taken at
        // acceptance — a switch after this point affects only the next
        // Turn.
        let mut mode = self.mode_default.get();
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
                });
                row.last_message_preview = Some(preview.chars().take(120).collect());
                row.last_message_at = Some(now);
                if row.title.is_none() {
                    row.title = Some(
                        preview
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("New chat")
                            .chars()
                            .take(TITLE_CHAR_LIMIT)
                            .collect(),
                    );
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
        persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .map_err(|error| RpcError::Failed(error.to_string()))?;
        if let Some(baseline) = baseline {
            self.turns.insert(chat_id, baseline);
        }
        chat.transcript
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(SessionMessageEntry {
                id: message_id,
                role: MessageRole::User,
                parts,
                created_at: timestamp,
                device_id: self.engine_info.device_id.clone(),
                status: None,
                continuation_of: None,
            });
        self.runtime.publish_chats();
        self.runtime.set_session(chat_id, SessionStatus::Working);

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
        drop(persistence);
        chat.publish();

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
        RpcReply::value(&serde_json::json!({}))
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

    /// The settings record plus its live validation view — the reply shape
    /// of both title-settings RPCs.
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
        prompt: &str,
    ) -> Option<crate::title_task::TitleTaskSpec> {
        let settings = self.title_settings.get();
        let model_id = settings.model_id?;
        let (provider, _) = model_id.split_once('/')?;
        if !ProviderAdapter::is_eligible(provider) {
            return None;
        }
        let model = self.providers.resolve_model(provider, &model_id).ok()?;
        let api_key = self.providers.credentials.reveal_key(provider).await?;
        Some(crate::title_task::TitleTaskSpec {
            chat_id: chat_id.to_string(),
            data_dir: self.data_dir.clone(),
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
            if !ProviderAdapter::is_eligible(provider) {
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

fn required_string<'a>(params: &'a serde_json::Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcError::BadParams(format!("{field} is required")))
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
            methods::WATCH_MESSAGE_QUEUE => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::chat_id_is_path_safe(chat_id) {
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
            methods::CONTINUE_MESSAGE_QUEUE => {
                let chat_id = required_string(&params, "chatId")?;
                if !crate::store::chat_id_is_path_safe(chat_id) {
                    return Err(RpcError::BadParams("invalid chatId".into()));
                }
                let chat = self.runtime.chat(chat_id);
                let snapshot = {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    if chat.is_removed() {
                        return Err(RpcError::Failed("chat was deleted".into()));
                    }
                    queue.pause(false)?;
                    queue.snapshot()
                };
                self.kick_queue(chat);
                RpcReply::value(&snapshot)
            }
            methods::EDIT_QUEUED_MESSAGE => {
                let chat_id = required_string(&params, "chatId")?;
                let message_id = required_string(&params, "messageId")?;
                let prompt = required_string(&params, "prompt")?;
                if !crate::store::chat_id_is_path_safe(chat_id) {
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
            methods::DELETE_QUEUED_MESSAGE => {
                let chat_id = required_string(&params, "chatId")?;
                let message_id = required_string(&params, "messageId")?;
                if !crate::store::chat_id_is_path_safe(chat_id) {
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
            methods::GET_TITLE_SETTINGS => RpcReply::value(&self.title_settings_state().await),
            methods::SAVE_TITLE_SETTINGS => self.save_title_settings(params).await,
            // The composer's slash menu (ADR-0011): the one command this
            // backend intercepts itself.
            methods::LIST_COMMANDS => RpcReply::value(&serde_json::json!([
                { "name": "compact", "description": "Summarize the older conversation and keep only a recent tail" }
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
            methods::SUBSCRIBE_TERMINAL | methods::WATCH_CHECKOUT_CHANGE_REQUEST => {
                Ok(pending_stream())
            }

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
                        let Some(baseline) = self.turns.get(chat_id) else {
                            return Err(RpcError::Failed(
                                "no turn recorded for this chat yet".into(),
                            ));
                        };
                        let diff = self
                            .git
                            .turn_diff(cwd, &self.engine_info.device_id, &baseline)
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
                        let Some(baseline) = self.turns.get(chat_id) else {
                            return Err(RpcError::Failed(
                                "no turn recorded for this chat yet".into(),
                            ));
                        };
                        let text = self
                            .git
                            .turn_file_text(
                                &request.cwd,
                                &self.engine_info.device_id,
                                &request,
                                &baseline,
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
