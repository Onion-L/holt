//! The RPC surface: `RpcService` dispatch plus the space/chat mutation and
//! queue-command handlers it routes to.

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry, TranscriptFrame,
    diff_transcript,
};
use holt_proto::{
    AuthState, Chat, ChatConfig, RunRequest, SessionStatus, Space, TitleSettings,
    TitleSettingsState, TitleSource,
};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentRun, ChatRuntime, run_agent_command};
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
                    queue.enqueue(request, message_id)?;
                }
                self.kick_queue(chat);
            }
            SessionCommandPayload::InvokeSkill {
                request,
                name,
                extra_instructions,
                message_id,
            } => {
                let execution = chat
                    .execution
                    .clone()
                    .try_lock_owned()
                    .map_err(|_| RpcError::Failed("this chat is already running".into()))?;
                let cancel = CancellationToken::new();
                *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
                // Resolve against a fresh catalog FIRST: an unknown (or
                // shadowed/invalid) name fails at submit time with no run
                // and no transcript entry (ADR-0006).
                let Some(skill) = self.skills.resolve(Some(&request.cwd), &name).await else {
                    *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    return Err(RpcError::Failed(format!("unknown skill: {name}")));
                };
                let block = crate::skills::invocation_prompt(&skill, None);
                let prompt =
                    crate::skills::invocation_prompt(&skill, extra_instructions.as_deref());
                // The user entry keeps the compact chip (name + source
                // pointer); the `<skill>` block rides the AGENT entry's
                // opening chip instead — the reply opens with what the
                // model was told to follow, ahead of any thinking. The raw
                // `/skill` directive never appears anywhere.
                let mut parts = vec![MessagePart::Skill {
                    id: "t0".into(),
                    name: skill.name.clone(),
                    file: skill.file_path.clone(),
                    content: None,
                }];
                if let Some(extra) = extra_instructions
                    .clone()
                    .filter(|extra| !extra.trim().is_empty())
                {
                    parts.push(MessagePart::Text {
                        id: "t1".into(),
                        text: extra,
                    });
                }
                let preview = format!("/skill {name}");
                let invocation_seed = MessagePart::Skill {
                    id: "s0".into(),
                    name: skill.name.clone(),
                    file: skill.file_path.clone(),
                    content: Some(block),
                };
                let run = self
                    .start_turn(
                        &params.chat_id,
                        chat.clone(),
                        request,
                        message_id,
                        parts,
                        preview,
                        prompt,
                        Some(invocation_seed),
                        None,
                        cancel,
                        false,
                    )
                    .await;
                let run = match run {
                    Ok(run) => run,
                    Err(error) => {
                        *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        return Err(error);
                    }
                };
                let running_chat = chat.clone();
                let runtime = self.runtime.clone();
                let task = tokio::spawn(async move {
                    let _execution = execution;
                    let interrupted = run.cancel.clone();
                    let success = run_agent_command(run).await;
                    if !success && !interrupted.is_cancelled() {
                        let mut queue =
                            running_chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                        if !running_chat.is_removed() {
                            let _ = queue.pause(true);
                        }
                    }
                    runtime.set_session(
                        &running_chat.chat_id,
                        if success || interrupted.is_cancelled() {
                            SessionStatus::Idle
                        } else {
                            SessionStatus::Errored
                        },
                    );
                });
                chat.track_task(&task);
            }
            SessionCommandPayload::Steer { .. } => {
                return Err(RpcError::Failed("steering is not available yet".into()));
            }
            SessionCommandPayload::RespondInput { .. } => {
                return Err(RpcError::Failed(
                    "input responses are not available yet".into(),
                ));
            }
            SessionCommandPayload::Compact { request } => {
                self.compact(chat, request).await?;
            }
        }
        RpcReply::value(&serde_json::json!({}))
    }

    /// A manual `/compact` (ADR-0011): compaction on demand — never a
    /// Turn. Nothing Turn-scoped is stamped or reset; the session enters
    /// `Compacting` (same interrupt affordance as a run) and returns to
    /// idle on completion, failure, or interruption.
    async fn compact(&self, chat: Arc<ChatRuntime>, request: RunRequest) -> Result<(), RpcError> {
        let execution = chat
            .execution
            .clone()
            .try_lock_owned()
            .map_err(|_| RpcError::Failed("this chat is already running".into()))?;
        // Slash commands remain direct until ticket 04; they share the
        // ordinary queue's execution boundary.
        if chat
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some()
        {
            return Err(RpcError::Failed("this chat is already running".into()));
        }
        // A History that fits the retained tail has nothing to compact:
        // refuse before any status change or model request.
        let history = chat
            .history
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !crate::compaction::has_compactable_content(&history) {
            return Err(RpcError::Failed("There is nothing to compact".into()));
        }
        let model = self
            .providers
            .resolve_model(request.provider.as_str(), &request.model)
            .map_err(RpcError::BadParams)?;
        let cancel = CancellationToken::new();
        *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        let Some(api_key) = self
            .providers
            .credentials
            .reveal_key(request.provider.as_str())
            .await
        else {
            *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(RpcError::Failed(format!(
                "provider {} is not configured",
                request.provider
            )));
        };
        if cancel.is_cancelled() || chat.is_removed() {
            *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Err(RpcError::Failed(
                "Compaction interrupted before execution".into(),
            ));
        }
        let stream_fn = self
            .runtime
            .stream_fn
            .clone()
            .unwrap_or_else(crate::agent::default_stream_fn);
        self.runtime
            .set_session(&chat.chat_id, SessionStatus::Compacting);

        let runtime = self.runtime.clone();
        let device_id = self.engine_info.device_id.clone();
        let compacting_chat = chat.clone();
        let compacting_chat_id = chat.chat_id.clone();
        let task = tokio::spawn(async move {
            let _execution = execution;
            let outcome = crate::compaction::compact_now(
                &history,
                &model,
                &stream_fn,
                &api_key,
                holt_doc::parts::CompactionTrigger::Manual,
                Some(&cancel),
            )
            .await;
            match outcome {
                Ok(Some(outcome)) => {
                    crate::agent::record_turn_start_compaction(
                        &compacting_chat,
                        &device_id,
                        &outcome.record,
                    );
                    *compacting_chat
                        .history
                        .write()
                        .unwrap_or_else(|e| e.into_inner()) = outcome.messages;
                    // A manual compaction pays the overflow debt too.
                    runtime.take_compact_before_next_turn(&compacting_chat_id);
                }
                // Pre-checked at acceptance; losing the race just settles.
                Ok(None) => {}
                Err(reason) => {
                    // An interruption is the user's own act — settle
                    // quietly. A real failure surfaces on the Transcript;
                    // the History is untouched either way.
                    if !cancel.is_cancelled() {
                        {
                            let mut queue = compacting_chat
                                .queue
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if !compacting_chat.is_removed() {
                                let _ = queue.pause(true);
                            }
                        }
                        tracing::warn!(target: "holt::compaction", %reason, "manual compaction failed");
                        crate::agent::push_system_part(
                            &compacting_chat,
                            &device_id,
                            format!("compaction-failed-{}", uuid::Uuid::new_v4()),
                            holt_doc::MessagePart::Notice {
                                id: "n0".into(),
                                message: format!(
                                    "Compaction failed ({reason}); the conversation was \
                                     left unchanged."
                                ),
                            },
                        );
                    }
                }
            }
            *compacting_chat
                .cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            runtime.set_session(&compacting_chat_id, SessionStatus::Idle);
        });
        chat.track_task(&task);
        Ok(())
    }

    /// Accept and launch one ordinary Turn — the shared tail of `Run` and
    /// `InvokeSkill`. `parts` is the transcript user entry (prompt text or
    /// skill chip), `preview` the sidebar/title text, `prompt` the
    /// model-visible text, and `invocation` the chip seeded at the head of
    /// the run's own entry (skill invocations only).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_turn(
        &self,
        chat_id: &str,
        chat: Arc<ChatRuntime>,
        request: RunRequest,
        message_id: String,
        parts: Vec<MessagePart>,
        preview: String,
        prompt: String,
        invocation: Option<MessagePart>,
        title_prompt: Option<String>,
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
        if queued {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() || chat.is_removed() {
                return Err(RpcError::Failed("Turn interrupted before execution".into()));
            }
            queue.start(&message_id, timestamp)?;
        }
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

            // Everything this backend has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
