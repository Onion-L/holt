//! The RPC surface: `RpcService` dispatch plus the space/chat mutation and
//! queue-command handlers it routes to.

use async_trait::async_trait;
use chrono::Utc;
use holt_doc::{
    MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry, TranscriptFrame,
    diff_transcript,
};
use holt_proto::{AuthState, Chat, ChatConfig, RunRequest, SessionStatus, Space};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use pi_core::ai::auth::types::CredentialStore;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::LocalEngine;
use crate::agent::{AgentRun, ChatRuntime, run_agent_command};
use crate::local_fs::{list_drives, list_folders, local_device};
use crate::providers::ProviderAdapter;
use crate::store::{persist_chats, persist_spaces};

impl LocalEngine {
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
                let prompt = request.prompt.clone();
                let parts = vec![MessagePart::Text {
                    id: "t0".into(),
                    text: prompt.clone(),
                }];
                self.start_turn(
                    &params.chat_id,
                    chat,
                    request,
                    message_id,
                    parts,
                    prompt.clone(),
                    prompt,
                    None,
                )
                .await?;
            }
            SessionCommandPayload::InvokeSkill {
                request,
                name,
                extra_instructions,
                message_id,
            } => {
                // Resolve against a fresh catalog FIRST: an unknown (or
                // shadowed/invalid) name fails at submit time with no run
                // and no transcript entry (ADR-0006).
                let Some(skill) = self.skills.resolve(Some(&request.cwd), &name).await else {
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
                self.start_turn(
                    &params.chat_id,
                    chat,
                    request,
                    message_id,
                    parts,
                    preview,
                    prompt,
                    Some(invocation_seed),
                )
                .await?;
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
        // Refused while a Turn runs, with the same message as a second
        // prompt — queueing belongs to the future message queue.
        if chat
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|token| !token.is_cancelled())
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
        let stream_fn = self
            .runtime
            .stream_fn
            .clone()
            .unwrap_or_else(crate::agent::default_stream_fn);
        let cancel = CancellationToken::new();
        *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        self.runtime
            .set_session(&chat.chat_id, SessionStatus::Compacting);

        let runtime = self.runtime.clone();
        let device_id = self.engine_info.device_id.clone();
        let compacting_chat = chat.clone();
        let compacting_chat_id = chat.chat_id.clone();
        tokio::spawn(async move {
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
        Ok(())
    }

    /// Accept and launch one ordinary Turn — the shared tail of `Run` and
    /// `InvokeSkill`. `parts` is the transcript user entry (prompt text or
    /// skill chip), `preview` the sidebar/title text, `prompt` the
    /// model-visible text, and `invocation` the chip seeded at the head of
    /// the run's own entry (skill invocations only).
    #[allow(clippy::too_many_arguments)]
    async fn start_turn(
        &self,
        chat_id: &str,
        chat: Arc<ChatRuntime>,
        request: RunRequest,
        message_id: String,
        parts: Vec<MessagePart>,
        preview: String,
        prompt: String,
        invocation: Option<MessagePart>,
    ) -> Result<(), RpcError> {
        // Turn baseline FIRST (ADR-0003): captured synchronously at
        // acceptance, before validation and before the run starts —
        // even a run rejected for a bogus provider records the
        // turn's starting point.
        if let Ok(baseline) = self.git.turn_baseline(&request.cwd).await {
            self.turns.insert(chat_id, baseline);
        }
        // Turn identity (ADR-0007): beside the baseline, restamp the chat
        // row's cwd from the request and stamp its branch + source context
        // from the working directory's live HEAD — synchronously at
        // acceptance, so a run rejected further down still records where its
        // Turn would run. Non-git folders stamp only the cwd. A chat whose
        // Turn is still live does NOT restamp (a mid-run send fails loudly
        // and must not move the label off the running Turn's branch); the
        // atomic running check further down still guards the spawn itself.
        let chat_running = chat
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|token| !token.is_cancelled());
        let stamped = if chat_running {
            false
        } else {
            let source = self
                .git
                .turn_source_context(&request.cwd, &self.engine_info.device_id)
                .await;
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let mut stamped = false;
            if let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) {
                row.cwd = Some(request.cwd.clone());
                if let Some(source) = source.as_ref() {
                    row.branch = Some(source.branch.clone());
                    row.source_context = Some(source.clone());
                }
                stamped = true;
            }
            stamped
        };
        if stamped {
            persist_chats(
                &self.data_dir,
                &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
            )
            .map_err(|error| RpcError::Failed(error.to_string()))?;
            self.runtime.publish_chats();
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
                parts,
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
            if let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) {
                // cwd + branch + source context already restamped at
                // acceptance, above — this pass records the run's config
                // and sidebar bookkeeping.
                row.config = Some(ChatConfig {
                    provider: request.provider.clone(),
                    model: request.model.clone(),
                    reasoning: request.reasoning,
                    model_options: request.model_options.clone(),
                    sandbox: request.sandbox,
                });
                row.last_message_preview = Some(preview.chars().take(120).collect());
                row.last_message_at = Some(now);
                if row.title.is_none() {
                    row.title = Some(
                        preview
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
        self.runtime.set_session(chat_id, SessionStatus::Working);

        let runtime = self.runtime.clone();
        let chat_id = chat_id.to_string();
        tokio::spawn(run_agent_command(AgentRun {
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
            stream_fn: self.runtime.stream_fn.clone(),
        }));
        Ok(())
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

            // Everything this backend has no data for — mutations, terminals,
            // repos, uploads — reports as an unknown method: that is the
            // wire convention the UI already treats as "this engine doesn't
            // serve it yet" and degrades gracefully on.
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}
