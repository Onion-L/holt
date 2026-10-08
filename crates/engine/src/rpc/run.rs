//! The queue/Turn lifecycle: queue commands, attended sends, worktree
//! intents, the queue watches, and the run entry points.

use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use holt_proto::{
    Chat, ChatConfig, PendingKind, RunOutcome, RunRequest, SessionStatus, Space, TitleSource,
};
use holt_rpc::{RpcError, RpcReply};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

use super::required_string;
use crate::EngineService;
use crate::agent::{AgentRun, ChatRuntime};
use crate::store::persist_spaces;

impl EngineService {
    /// A queued run built from the chat's stored config with a prompt
    /// swapped in — the one shape every engine-side enqueue uses (the
    /// composer's sends arrive pre-built; the plan follow-up and the Key
    /// request's settle notices build here).
    pub(super) fn queued_run_request(
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
    fn attended_send(chat: &ChatRuntime, queue: &crate::queue::Queue) -> bool {
        queue.paused() && queue.idle() && !chat.driver_running.load(Ordering::Acquire)
    }

    /// The one engine-side enqueue path for a run command: the composer's
    /// `Run` and the Key request's settle notice both queue through here,
    /// so attended-send and admission semantics cannot drift apart.
    pub(super) fn enqueue_run(
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
        // Session worktrees (ADR-0038): the isolation intent persists at
        // command acceptance, before the run becomes durable queue work — a
        // crash between the ledger write and admission must not drop it
        // (recovery clears `started` without re-reading the spec).
        if let Some(spec) = &request.worktree {
            self.absorb_worktree_intent(&chat, spec)?;
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

    /// Persist the session-worktree isolation intent at acceptance: an unset
    /// intent fills, an equal one is idempotent, a different one is rejected —
    /// the intent is chat-owned and never overwritten. A registry row is
    /// minted when `createChat` never arrived (the mutate is best-effort), so
    /// the run cannot land on a rowless chat (ADR-0038).
    fn absorb_worktree_intent(
        &self,
        chat: &Arc<ChatRuntime>,
        spec: &holt_proto::WorktreeSpec,
    ) -> Result<(), RpcError> {
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        let _chats_store = self
            .runtime
            .chats_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|e| e.into_inner());
            match chats.iter_mut().find(|row| row.id == chat.chat_id) {
                Some(row) => match &row.worktree {
                    None => row.worktree = Some(spec.clone()),
                    Some(existing) if existing == spec => {}
                    Some(existing) => {
                        return Err(RpcError::BadParams(format!(
                            "chat is already bound to a different session worktree (base {} of {})",
                            existing.base, existing.repo_path
                        )));
                    }
                },
                None => chats.push(Chat {
                    id: chat.chat_id.clone(),
                    device_id: self.engine_info.device_id.clone(),
                    title: None,
                    title_source: TitleSource::Automatic,
                    title_task_started: false,
                    archived: false,
                    pinned: false,
                    cwd: Some(spec.repo_path.clone()),
                    branch: None,
                    checkout_id: None,
                    source_context: None,
                    config: None,
                    last_message_preview: None,
                    last_message_at: None,
                    created_at: Utc::now(),
                    space_id: None,
                    last_seen_at: None,
                    room_gen: None,
                    compact_before_next_turn: false,
                    plan_mode: None,
                    provider_mode: false,
                    worktree: Some(spec.clone()),
                    routine_run: None,
                }),
            }
        }
        crate::store::persist_chats(
            &self.data_dir,
            &self.runtime.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();
        Ok(())
    }

    /// The chat's persisted isolation intent, if any — the single source the
    /// admission resolves the working directory from.
    fn worktree_intent(&self, chat_id: &str) -> Option<holt_proto::WorktreeSpec> {
        let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
        chats
            .iter()
            .find(|row| row.id == chat_id)
            .and_then(|row| row.worktree.clone())
    }

    /// The chat's dedicated worktree Space (ADR-0038): an ensure-style lookup
    /// over the same registry `createSpace` serves. The same id at the same
    /// path, or the same (device, path) under any id, reuses the existing row;
    /// the same id at a DIFFERENT path is registration damage and fails the
    /// Turn. Persisting the Space before the chat re-parents keeps a
    /// mid-failure world consistent — an unclaimed Space row is harmless and
    /// gets reused.
    fn ensure_worktree_space(&self, chat_id: &str, worktree_path: &str) -> Result<Space, RpcError> {
        let space_id = format!("wt-{chat_id}");
        let device_id = self.engine_info.device_id.clone();
        let git_dir = crate::git::discover_git_dir(std::path::Path::new(worktree_path));
        let checkout_id = git_dir
            .as_ref()
            .map(|git_dir| crate::git::checkout_identity(&device_id, git_dir));
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| RpcError::Failed("spaces lock poisoned".into()))?;
        if let Some(index) = spaces.iter().position(|space| space.id == space_id) {
            let existing = &mut spaces[index];
            if existing.path != worktree_path {
                return Err(RpcError::Failed(format!(
                    "worktree space {space_id} is bound to another path ({})",
                    existing.path
                )));
            }
            let changed =
                existing.git_detected != git_dir.is_some() || existing.checkout_id != checkout_id;
            if changed {
                existing.git_detected = git_dir.is_some();
                existing.git_checked_at = Some(Utc::now());
                existing.checkout_id = checkout_id.clone();
                let refreshed = existing.clone();
                persist_spaces(&self.data_dir, &spaces)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                let value = serde_json::to_value(&*spaces)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.spaces_tx.send_replace(value);
                return Ok(refreshed);
            }
            return Ok(existing.clone());
        }
        if let Some(index) = spaces
            .iter()
            .position(|space| space.device_id == device_id && space.path == worktree_path)
        {
            let existing = &mut spaces[index];
            let changed =
                existing.git_detected != git_dir.is_some() || existing.checkout_id != checkout_id;
            if changed {
                existing.git_detected = git_dir.is_some();
                existing.git_checked_at = Some(Utc::now());
                existing.checkout_id = checkout_id.clone();
                let refreshed = existing.clone();
                persist_spaces(&self.data_dir, &spaces)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                let value = serde_json::to_value(&*spaces)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.spaces_tx.send_replace(value);
                return Ok(refreshed);
            }
            return Ok(existing.clone());
        }
        let space = Space {
            id: space_id,
            device_id,
            path: worktree_path.to_string(),
            name: None,
            git_detected: git_dir.is_some(),
            git_checked_at: None,
            checkout_id: checkout_id.clone(),
            created_at: Utc::now(),
        };
        spaces.push(space.clone());
        persist_spaces(&self.data_dir, &spaces).map_err(|error| {
            spaces.pop();
            RpcError::Failed(error.to_string())
        })?;
        let value =
            serde_json::to_value(&*spaces).map_err(|error| RpcError::Failed(error.to_string()))?;
        self.spaces_tx.send_replace(value);
        Ok(space)
    }

    /// Persist the visible record of a pre-execution preparation failure
    /// (ADR-0038): the user entry exactly as admission would have written it,
    /// plus one system Error entry carrying the reason. No Turn runs, so no
    /// History entry exists for the user message — the edit path treats such
    /// entries as transcript-only.
    fn persist_preparation_failure(
        &self,
        chat: Arc<ChatRuntime>,
        message_id: &str,
        parts: Vec<MessagePart>,
        timestamp: i64,
        reason: &str,
    ) {
        let user = SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts,
            created_at: timestamp,
            device_id: self.engine_info.device_id.clone(),
            status: None,
            continuation_of: None,
        };
        let error_entry = SessionMessageEntry {
            id: format!("{message_id}-prep-error"),
            role: MessageRole::System,
            parts: vec![MessagePart::Error {
                id: "e0".into(),
                message: reason.to_string(),
            }],
            created_at: timestamp,
            device_id: self.engine_info.device_id.clone(),
            status: None,
            continuation_of: None,
        };
        {
            let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
            match transcript.iter_mut().find(|entry| entry.id == message_id) {
                Some(slot) => *slot = user,
                None => transcript.push(user),
            }
            transcript.push(error_entry);
        }
        chat.persist_entry(message_id);
        chat.persist_entry(&format!("{message_id}-prep-error"));
        self.runtime.publish_chats();
    }

    pub(super) async fn queue_command(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
                if let Some(id) = message_id {
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.promote(&id)?;
                } else {
                    let mut request = request
                        .ok_or_else(|| RpcError::BadParams("steer request is required".into()))?;
                    if prompt.trim().is_empty() {
                        return Err(RpcError::BadParams("prompt must not be empty".into()));
                    }
                    request.prompt = prompt;
                    if let Some(spec) = &request.worktree {
                        self.absorb_worktree_intent(&chat, spec)?;
                    }
                    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.enqueue_priority(request, uuid::Uuid::new_v4().to_string())?;
                }
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
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
                if let Some(spec) = &request.worktree {
                    self.absorb_worktree_intent(&chat, spec)?;
                }
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
        mut request: RunRequest,
        message_id: String,
        mut parts: Vec<MessagePart>,
        mut preview: String,
        mut prompt: String,
        mut title_prompt: Option<String>,
        cancel: CancellationToken,
        queued: bool,
    ) -> Result<AgentRun, RpcError> {
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
        let mut mentions = false;
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
                    mentions = true;
                }
                // Manual Compaction is admitted by the driver itself — it
                // never becomes a Turn (ADR-0011).
                PendingKind::Compact => unreachable!("Compaction never enters start_turn"),
            }
        }

        // Session worktrees (ADR-0038): the chat's persisted isolation intent
        // — not the request's cwd — decides where this run executes. The
        // request carries no spec on later sends, so a message queued behind
        // the creating one, or sent after a failed materialization, still
        // resolves through the intent and can never fall back to the main
        // checkout. Materialization happens at drain time.
        let intent = self.worktree_intent(chat_id);
        let worktree_chat = intent.is_some();
        let mut preparation_failure: Option<String> = None;
        if let Some(spec) = intent {
            match self
                .git
                .materialize_worktree(
                    &spec.repo_path,
                    &spec.base,
                    chat_id,
                    &self.data_dir.join("worktrees"),
                )
                .await
            {
                Ok(worktree_cwd) => {
                    // A Stop during materialization must not launch the Turn
                    // afterwards; the worktree itself stays for reuse.
                    if cancel.is_cancelled() || chat.is_removed() {
                        return Err(RpcError::Failed("Turn interrupted before execution".into()));
                    }
                    request.cwd = worktree_cwd;
                }
                Err(error) => preparation_failure = Some(error),
            }
        }
        if preparation_failure.is_none() && (cancel.is_cancelled() || chat.is_removed()) {
            return Err(RpcError::Failed("Turn interrupted before execution".into()));
        }
        let worktree_space = if worktree_chat && preparation_failure.is_none() {
            match self.ensure_worktree_space(chat_id, &request.cwd) {
                Ok(space) => Some(space),
                Err(error) => {
                    preparation_failure = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };
        // Inline mentions resolve against a fresh catalog at admission —
        // against the RESOLVED working directory, so a worktree chat's
        // project skills come from the worktree: resolved `<skill>` blocks
        // prepend the model-visible prompt and seed the run entry's opening
        // chips; unresolved mentions stay ordinary text.
        if mentions && preparation_failure.is_none() {
            let (model_prompt, chips) = self
                .skills
                .resolve_prompt_mentions(&request.cwd, &prompt)
                .await;
            prompt = model_prompt;
            invocation = chips;
        }
        if let Some(error) = preparation_failure {
            let reason = format!("Session worktree could not be prepared: {error}");
            self.persist_preparation_failure(chat.clone(), &message_id, parts, timestamp, &reason);
            return Err(RpcError::Failed(reason));
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
                } else {
                    row.branch = None;
                    row.source_context = None;
                }
                // Session worktree (ADR-0038): re-parent to the worktree
                // Space and stamp the worktree's own checkout identity —
                // Changes matches diffs by checkout_id first, so a stale id
                // would keep matching the main checkout.
                if let Some(space) = &worktree_space {
                    row.space_id = Some(space.id.clone());
                    row.checkout_id = space.checkout_id.clone();
                }
                planning = row.plan_mode.is_some();
                provider_mode = row.provider_mode;
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
                let scope = row
                    .config
                    .as_ref()
                    .map(|config| config.scope)
                    .unwrap_or_default();
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
        // A run waiting on a question runs again with the next Turn.
        self.routines
            .move_run(chat_id, RunOutcome::Waiting, RunOutcome::Running);
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
        // That write is the storage fence (issue #16): if the user entry
        // could not be saved, the Turn never launches — no model call on
        // storage the run cannot durably record. The item returns to the
        // queue parked with the refusal, so the actionable text reaches
        // the queue panel and survives a restart.
        if let Some(error) = chat
            .persistence_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let reason = format!(
                "Conversation could not be saved ({error}). Restore storage and reopen Holt before continuing."
            );
            let retracted = {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.retract(&message_id, reason.clone())
            };
            retracted?;
            return Err(RpcError::Failed(reason));
        }

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

    /// Replace the latest user message and run it again. Drafting stays in
    /// the UI; submission is the serialization point that cancels the old
    /// Turn, prunes both records, and requeues the replacement ahead of any
    /// remaining work.
    pub(super) async fn edit_last_message(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
        // A user entry with no History counterpart never ran (pre-execution
        // preparation failure, ADR-0038): the edit is transcript-only —
        // History has no tail for this message to truncate.
        let history_index = history
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| {
                matches!(message, pi_core::agent::types::AgentMessage::User(user) if user.timestamp == target.created_at)
            })
            .map(|(index, _)| index);
        if let Some(history_index) = history_index {
            history.truncate(history_index);
        }
        if let Err(error) = crate::store::rewrite_transcript(&self.data_dir, chat_id, &entries) {
            drop(history);
            self.resume_after_edit_failure(&chat, attended);
            return Err(RpcError::Failed(error.to_string()));
        }
        if history_index.is_some()
            && let Err(error) = crate::history::rewrite(&self.data_dir, chat_id, &history)
        {
            drop(history);
            self.resume_after_edit_failure(&chat, attended);
            return Err(RpcError::Failed(error.to_string()));
        }
        drop(history);
        *chat.transcript.write().unwrap_or_else(|e| e.into_inner()) = entries;
        // The log was rewritten whole: the incremental writers' anchors are
        // gone, so every entry's next persist is a full line again.
        chat.clear_persisted_parts();
        // The pruned Turn's change set dies with it: the card attaches to a
        // Turn the transcript no longer holds (ADR-0024), so retract the
        // persisted record and the in-memory baselines BEFORE the publish —
        // a consumer restoring cards off the reset frame must never revive
        // the stale set. The working tree keeps its edits regardless.
        crate::turn_change_store::delete(&self.data_dir, chat_id, message_id);
        self.turn_changes.retract(chat_id, message_id);
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

    /// The model config and cwd a queued follow-up runs with: exactly
    /// what the composer would send on this chat.
    pub(super) fn chat_run_target(
        &self,
        chat_id: &str,
    ) -> Result<(holt_proto::ChatConfig, String), RpcError> {
        let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
        let row = chats
            .iter()
            .find(|row| row.id == chat_id)
            .ok_or_else(|| RpcError::Failed("chat was deleted".into()))?;
        let config = row
            .config
            .clone()
            .ok_or_else(|| RpcError::Failed("the chat has no model config".into()))?;
        let cwd = row
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
        Ok((config, cwd))
    }

    pub(super) fn watch_message_queue(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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

    pub(super) fn watch_chat_usage(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
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

    pub(super) async fn usage_stats(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
        let reply =
            tokio::task::spawn_blocking(move || crate::usage_stats::stats(&data_dir, params.days))
                .await
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        RpcReply::value(&reply)
    }

    pub(super) fn watch_turn_terminal_events(&self) -> Result<RpcReply, RpcError> {
        let stream =
            futures::stream::unfold(self.turn_events.subscribe(), |mut receiver| async move {
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
            });
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    pub(super) fn watch_turn_retry(&self) -> Result<RpcReply, RpcError> {
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

    pub(super) fn continue_message_queue(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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

    pub(super) fn edit_queued_message(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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

    pub(super) fn delete_queued_message(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}
