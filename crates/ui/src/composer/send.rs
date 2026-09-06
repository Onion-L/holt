//! The send path: ordinary-message enqueue, direct slash commands,
//! durable-delivery retries, and Stop/interrupt.

use super::Composer;
use super::send_mode::{SendButtonMode, composer_has_content, send_button_mode};

use gpui::{App, Context, div, prelude::*, px};

use holt_doc::{MessagePart, SessionCommandPayload, SessionMessageEntry};
use holt_proto::RunRequest;
use holt_rpc::methods;

use crate::attachments::{self};
use crate::state::Indicator;
use crate::theme::Theme;

fn failure_restore_text(parsed: &super::slash::Parsed, typed: String) -> Option<String> {
    (!matches!(parsed, super::slash::Parsed::Compact)).then_some(typed)
}

impl Composer {
    pub(super) fn run_live(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        let Some(chat_id) = s.selected_chat.as_deref() else {
            return false;
        };
        matches!(
            s.indicator_for(chat_id, chrono::Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        )
    }

    /// New-chat sends need a project: with none picked (empty device, or a
    /// selection healed away) the send button dims and submit is a no-op —
    /// project-less `~`-cwd sessions are no longer mintable from the canvas.
    /// Existing chats carry their own project, so they always send.
    fn send_blocked(&self, cx: &App) -> bool {
        if self.failed_submissions.contains_key(&self.current_key) {
            return true;
        }
        let state = self.state.read(cx);
        if state.selected_chat.is_some() {
            return self.sending || !self.pickers.read(cx).can_send(cx);
        }
        // New-chat canvas: needs a project and a configured provider/model.
        self.sending || state.selected_space_row().is_none() || !self.pickers.read(cx).can_send(cx)
    }

    pub(super) fn button_mode(&self, cx: &App) -> SendButtonMode {
        let has_text = composer_has_content(
            self.input.read(cx).text(),
            self.staged().len(),
            self.staged_comments(cx).len(),
        );
        send_button_mode(self.run_live(cx), has_text)
    }

    pub(super) fn on_submit(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            // Enter inside the panel's free-text input submits the page.
            let typed = self.input.read(cx).text().trim().to_string();
            if let Some(w) = self.wizard.as_mut() {
                w.set_typed(typed);
            }
            self.wizard_advance(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        self.submit_text(text, cx);
    }

    /// Submit text that may have come from the slash popup instead of the
    /// input. Keeping this separate lets commands such as `/compact` dispatch
    /// without briefly filling the composer first.
    pub(super) fn submit_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        if self
            .failed_submissions
            .get(&self.current_key)
            .is_some_and(|(original, _)| original == &text)
        {
            self.retry_submission(cx);
            return;
        }
        // Slash commands are handled by the composer itself (ADR-0006):
        // a recognized-but-nameless `/skill` never reaches the prompt
        // path — surface the usage instead. Same for `/compact` with
        // arguments (ADR-0011 — it takes none).
        match super::slash::parse(&text) {
            super::slash::Parsed::Malformed => {
                self.failure = Some("Usage: /skill <name> [extra instructions]".into());
                self.failure_key = None;
                cx.notify();
                return;
            }
            super::slash::Parsed::MalformedCompact => {
                self.failure = Some("Usage: /compact (no arguments)".into());
                self.failure_key = None;
                cx.notify();
                return;
            }
            // A skill switched off in Settings → Skills is refused even when
            // typed out in full — disabled means unusable, not just hidden.
            super::slash::Parsed::Skill { name, .. }
                if crate::settings::current(cx).disabled_skills.contains(&name) =>
            {
                self.failure = Some(
                    format!("Skill \"{name}\" is disabled — re-enable it in Settings → Skills")
                        .into(),
                );
                self.failure_key = None;
                cx.notify();
                return;
            }
            _ => {}
        }
        let no_content =
            !composer_has_content(&text, self.staged().len(), self.staged_comments(cx).len());
        match self.button_mode(cx) {
            SendButtonMode::Stop => self.interrupt(cx),
            _ if no_content => {}
            _ if self.send_blocked(cx) => {}
            SendButtonMode::Send | SendButtonMode::Queue => self.send(text, cx),
        }
    }
    /// Durably enqueue an ordinary message. New chats
    /// thread the picked config in: worktree creation (when the isolated toggle
    /// is on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
    /// reasoning / options on the Run request itself (§1.7).
    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            self.failure_key = None; // global — meaningful on every chat
            cx.notify();
            return;
        };
        // Chat id: existing selection, or client-minted for the new-chat canvas
        // (the chat then appears from the doc host once the doc materializes).
        let (chat_id, is_new) = match self.state.read(cx).selected_chat.clone() {
            Some(id) => (id, false),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        // Where the new session runs (Current checkout / reuse an existing
        // worktree / fresh worktree off the picked base) — resolved NOW so
        // the async block needs no picker access.
        let plan = self.pickers.read(cx).checkout_plan();
        // Fully-resolved model/reasoning/options — concrete values (chat config
        // or defaults), so the engine never has to guess a "default".
        let resolved = self.pickers.read(cx).resolved(cx);
        let existing_cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.cwd.clone());
        // The chat's permission mode rides the Run request (ADR-0014): an
        // existing chat carries its stored mode; a fresh row takes the draft
        // pick when one was made, else defaults to confirm-changes. (The
        // field is advisory — the engine preserves the STORED mode; the
        // draft pick lands authoritatively via setChatPermissionMode below.)
        let draft_mode = if is_new {
            self.pickers.read(cx).draft().permission_mode
        } else {
            None
        };
        let permission_mode = crate::pickers::resolve_permission_mode(
            self.state
                .read(cx)
                .selected_chat_row()
                .and_then(|c| c.config.as_ref().map(|config| config.permission_mode)),
            draft_mode,
        );
        let space = self.state.read(cx).selected_space_row().cloned();
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let device_id = if is_new {
            local_device_id
                .clone()
                .unwrap_or_else(|| "local".to_string())
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
                .or_else(|| local_device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        };
        let space_id = space.as_ref().map(|s| s.id.clone());
        let space_path = space.as_ref().map(|s| s.path.clone());
        // Slash interception (ADR-0006): a parsed `/skill` rides this send
        // as a typed invocation — the raw directive never becomes prompt
        // text. A skill invocation travels alone: staged attachments and
        // diff-comment folding stay put for the next ordinary message.
        let slash = super::slash::parse(&text);
        // Both intercepted commands travel alone (ADR-0006/0011): staged
        // attachments and diff-comment folding stay put for the next
        // ordinary message.
        let travels_alone = matches!(
            slash,
            super::slash::Parsed::Skill { .. } | super::slash::Parsed::Compact
        );
        let staged = if travels_alone {
            Vec::new()
        } else {
            self.attachments
                .remove(&self.current_key)
                .unwrap_or_default()
        };
        // `typed` keeps the user's own words for the failure hand-back below:
        // restoring the folded prompt would paste the comment block into the
        // input as literal text.
        let key = self.current_key.clone();
        let comments = if travels_alone {
            Vec::new()
        } else {
            self.state.update(cx, |state, cx| {
                let taken = state.take_diff_comments(&key);
                if !taken.is_empty() {
                    cx.notify();
                }
                taken
            })
        };
        let typed = text.clone();
        let retry = self
            .failed_submissions
            .get(&chat_id)
            .filter(|(previous, _)| previous == &typed)
            .map(|(_, params)| params.clone());
        let submission_text = typed.clone();
        let text = if comments.is_empty() {
            text
        } else {
            crate::comments::with_comments(&text, &comments)
        };
        self.preview = None;
        let message_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().timestamp_millis();

        // Queued-attachment flow (durable-by-design): stage the bytes on the
        // local engine, then queue the command immediately with `pending://`
        // refs — the engine rewrites each ref to an absolute path once the
        // bytes land. Staging must never gate the queue (2026-08-19 incident:
        // a send died with a zombie peer link because the upload sat in front
        // of QueueCommand).
        let queued_flow = !staged.is_empty();
        // Upload identities minted NOW: in the queued flow the `pending://`
        // ref IS the persisted transport until the engine rewrites it, so the
        // id must exist before any bytes move.
        let upload_ids: Vec<String> = staged
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        // The echo carries attachment refs from the first frame, so photos
        // render while the send is still pending. Queued flow: the refs are
        // the real `pending://` identities (stable — no post-upload refresh).
        // Legacy flow: synthetic `pending/…` paths that the post-upload
        // refresh replaces with the engine's absolute paths. Either way the
        // staged bytes are seeded into the transcript cache under the chat's
        // device key.
        let echo_paths: Vec<String> = if queued_flow {
            staged
                .iter()
                .zip(&upload_ids)
                .map(|(att, id)| format!("pending://{id}/{}", att.name))
                .collect()
        } else {
            staged
                .iter()
                .map(|att| format!("pending/{}/{}", att.id, att.name))
                .collect()
        };
        let echo_text = attachments::with_attachments(&text, &echo_paths);
        // Queued flow also seeds the UPLOAD ALIAS: the engine rewrites the
        // persisted ref to `{its uploads dir}/{id8}-{name}` — an absolute
        // path the sender can't predict, but whose id8 it minted. The alias
        // keeps the thumbnail on the already-local bytes through that
        // rewrite instead of blanking into a reload skeleton.
        if queued_flow {
            for (upload_id, att) in upload_ids.iter().zip(&staged) {
                attachments::seed_attachment_alias(
                    &device_id,
                    upload_id,
                    &att.name,
                    att.image.clone(),
                );
            }
        }
        for (path, att) in echo_paths.iter().zip(&staged) {
            attachments::seed_attachment(&device_id, path, &att.name, att.image.clone());
        }

        // No optimistic echo: every submission is a typed queue item now
        // (ticket 04) — it shows in the queue panel immediately and enters
        // the Transcript only when the engine admits it, in the engine's own
        // frame. `/compact` likewise has no user entry to echo — the
        // Compacting status is the whole UI story until the divider lands
        // (ADR-0011).
        self.state.update(cx, |s, cx| {
            if is_new {
                s.select_chat(Some(chat_id.clone()), cx);
            }
            cx.notify();
        });

        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&self.current_key);
        self.failure = None;
        self.sending = true;
        cx.notify();

        let restore_text = failure_restore_text(&slash, typed);
        let err_chat_id = chat_id.clone();
        let err_message_id = message_id.clone();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<(), String> = async {
                // Attachments stage FIRST — before the chat row or anything
                // else exists. Staging is chat-independent (keyed by
                // uploadId), and ordering it first makes a new-chat send
                // atomic: a staging failure aborts with NOTHING created,
                // instead of stranding a just-minted empty chat (v0.2.12
                // "failed to stage → empty transcript" report).
                //
                // Queued flow: commit the bytes to the local engine's uploads
                // dir (fast, offline-safe) — the queued command carries the
                // `pending://` refs and the engine rewrites them to absolute
                // paths once the bytes land. Legacy flow: stage up front,
                // bounded by a total budget so a degraded engine fails the
                // send loudly instead of grinding through silent per-chunk
                // retries for minutes.
                let mut content = text.clone();
                let mut attachment_paths: Vec<String> = Vec::new();
                let mut transfers: Vec<serde_json::Value> = Vec::new();
                if !staged.is_empty() && queued_flow {
                    // Local staging is disk-speed; publish progress anyway so
                    // huge files still narrate.
                    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let total: u64 = staged.iter().map(|a| a.bytes().len() as u64).sum();
                    {
                        let progress = progress.clone();
                        this.update(cx, |composer, cx| {
                            composer.state.update(cx, |s, cx| {
                                s.begin_upload_progress(total, progress);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                    for (att, upload_id) in staged.iter().zip(&upload_ids) {
                        if let Err(err) = attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            upload_id,
                            att,
                            Some(progress.clone()),
                        )
                        .await
                        {
                            tracing::warn!(name = %att.name, error = %err, "local attachment stage failed");
                            return Err("Couldn't stage the attachment locally.".to_string());
                        }
                        transfers.push(serde_json::json!({
                            "uploadId": upload_id,
                            "fileName": att.name,
                        }));
                    }
                    // The echo refs ARE the persisted refs — no refresh pass.
                    attachment_paths = echo_paths.clone();
                    content = echo_text.clone();
                } else if !staged.is_empty() {
                    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let total: u64 = staged.iter().map(|a| a.bytes().len() as u64).sum();
                    {
                        let progress = progress.clone();
                        this.update(cx, |composer, cx| {
                            composer.state.update(cx, |s, cx| {
                                s.begin_upload_progress(total, progress);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                    for (att, upload_id) in staged.iter().zip(&upload_ids) {
                        match attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            upload_id,
                            att,
                            Some(progress.clone()),
                        )
                        .await
                        {
                            Ok(path) => attachment_paths.push(path),
                            Err(err) => {
                                tracing::warn!(name = %att.name, error = %err, "attachment upload failed");
                                return Err("Couldn't upload the attachment.".to_string());
                            }
                        }
                    }
                    // Seed the transcript cache from local bytes so the sent
                    // bubble's thumbnails never round-trip (seedTranscript-
                    // Attachment in the original send path).
                    for (path, att) in attachment_paths.iter().zip(&staged) {
                        attachments::seed_attachment(&device_id, path, &att.name, att.image.clone());
                    }
                    content = attachments::with_attachments(&text, &attachment_paths);
                    // Refresh the echo in place with the attachment refs
                    // (same id, same clock — the bubble grows its thumbnails
                    // without flickering).
                    let refreshed = SessionMessageEntry {
                        id: message_id.clone(),
                        role: holt_doc::MessageRole::User,
                        parts: vec![MessagePart::Text {
                            id: "t0".into(),
                            text: content.clone(),
                        }],
                        created_at,
                        device_id: "local".into(),
                        status: None,
                        continuation_of: None,
                    };
                    let echo_chat_id = chat_id.clone();
                    this.update(cx, |composer, cx| {
                        composer.state.update(cx, |s, cx| {
                            s.remove_echo(&echo_chat_id, &message_id);
                            s.push_echo(&echo_chat_id, refreshed);
                            cx.notify();
                        });
                    })
                    .ok();
                }

                // Resolve the working directory: existing chats keep theirs;
                // a new chat always runs in its SPACE's folder (ADR-0007 —
                // the reuse-worktree arm is gone), unless a fresh isolated
                // worktree is minted off the picked base ref on send (a
                // WorktreeSpec riding the Run command).
                let cwd = if is_new {
                    // Project-less sessions run from the home dir — "~" is
                    // expanded by the engine when the run spawns.
                    space_path.clone().or_else(|| Some("~".to_string()))
                } else {
                    existing_cwd
                }
                .unwrap_or_else(|| ".".to_string());
                // Fresh-worktree plans ride the QUEUED Run command (a
                // WorktreeSpec the engine materializes at drain time) instead
                // of a blocking CreateWorktree RPC here: the retired relay RPC
                // had no timeout, so a lost frame wedged the send on
                // "Sending…" forever while the session ran anyway (2026-08-18).
                let mut run_worktree: Option<holt_proto::WorktreeSpec> = None;
                // The picked ref rides createChat so the session footer names
                // it from the first frame (it read "Select ref" until the
                // engine's diff reconciler got around to stamping the branch).
                let mut chat_branch: Option<String> = None;
                if is_new {
                    match &plan {
                        crate::pickers::CheckoutPlan::CurrentCheckout { branch } => {
                            chat_branch = branch.clone();
                        }
                        crate::pickers::CheckoutPlan::NewWorktree { base } => {
                            // Footer shows the base until the engine stamps
                            // the actual holt/<name> branch post-creation. cwd
                            // stays the repo folder — an engine that doesn't
                            // know the spec degrades to the main checkout
                            // instead of failing the run.
                            chat_branch = base.clone();
                            if let Some(repo_path) = &space_path {
                                // The branch list may never arrive (engine
                                // still booting, a cold start) and the picker
                                // has no base. That must NOT silently drop the
                                // isolation the user picked (2026-08-19: "New
                                // worktree" ran in the main checkout): default
                                // to HEAD, which git resolves as the repo's
                                // current checkout state.
                                let base =
                                    base.clone().unwrap_or_else(|| "HEAD".to_string());
                                run_worktree = Some(holt_proto::WorktreeSpec {
                                    repo_path: repo_path.clone(),
                                    base,
                                });
                            }
                        }
                    }
                }

                // Best-effort Mutate createChat with the picked config: the
                // engine resolves device + cwd from the PROJECT row when one
                // is picked; project-less chats name the local device outright
                // (idempotent; the doc host would materialize the chat on
                // first command anyway, so failures are non-fatal).
                if is_new {
                    let mut mutate = serde_json::json!({
                        "op": "createChat",
                        "chatId": chat_id,
                    });
                    if let Some(object) = mutate.as_object_mut() {
                        match &space_id {
                            Some(space_id) => {
                                object.insert(
                                    "spaceId".into(),
                                    serde_json::Value::String(space_id.clone()),
                                );
                            }
                            None => {
                                object.insert(
                                    "deviceId".into(),
                                    serde_json::Value::String(device_id.clone()),
                                );
                            }
                        }
                    }
                    if let Some(object) = mutate.as_object_mut() {
                        if let Some(branch) = &chat_branch {
                            object.insert(
                                "branch".into(),
                                serde_json::Value::String(branch.clone()),
                            );
                        }
                        if let Some(config) = resolved.chat_config()
                            && let Ok(config) = serde_json::to_value(&config)
                        {
                            object.insert("config".into(), config);
                        }
                    }
                    if let Err(err) = attachments::call_with_timeout(
                        &engine,
                        cx.background_executor(),
                        methods::MUTATE,
                        mutate,
                        std::time::Duration::from_secs(30),
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "CreateChat mutate unavailable; doc host will materialize the chat");
                    }
                }

                // A permission mode picked on the canvas applies NOW — after
                // createChat, before the first Run is queued — so the first
                // Turn snapshots the chosen mode (the engine reads the mode at
                // run acceptance) and the sticky default is saved (ADR-0014).
                // Best-effort like createChat: a failure just means the chat
                // keeps the inherited default.
                if is_new && let Some(mode) = draft_mode {
                    crate::pickers::set_chat_permission_mode(
                        &engine,
                        cx.background_executor(),
                        &chat_id,
                        mode,
                    )
                    .await;
                }

                let command = match &slash {
                    // The engine builds the model-visible prompt from the
                    // skill's content; `request.prompt` rides empty.
                    super::slash::Parsed::Skill { name, extra } => {
                        SessionCommandPayload::InvokeSkill {
                            request: RunRequest {
                                prompt: String::new(),
                                provider: resolved.provider.clone().ok_or_else(|| {
                                    "Configure a provider before sending".to_string()
                                })?,
                                model: resolved.model.clone().ok_or_else(|| {
                                    "Choose a model before sending".to_string()
                                })?,
                                reasoning: resolved.reasoning,
                                model_options: resolved.model_options.clone(),
                                cwd,
                                permission_mode,
                                auto_approve: false,
                                attachments: Vec::new(),
                                worktree: run_worktree,
                            },
                            name: name.clone(),
                            extra_instructions: extra.clone(),
                            message_id: message_id.clone(),
                        }
                    }
                    // `/compact` rides the same queue as a typed command
                    // (ADR-0011); the engine needs only the provider/model
                    // resolution — its prompt is unused and the raw
                    // directive never reaches the model.
                    super::slash::Parsed::Compact => SessionCommandPayload::Compact {
                        request: RunRequest {
                            prompt: String::new(),
                            provider: resolved.provider.clone().ok_or_else(|| {
                                "Configure a provider before sending".to_string()
                            })?,
                            model: resolved
                                .model
                                .clone()
                                .ok_or_else(|| "Choose a model before sending".to_string())?,
                            reasoning: resolved.reasoning,
                            model_options: resolved.model_options.clone(),
                            cwd,
                            permission_mode,
                            auto_approve: false,
                            attachments: Vec::new(),
                            worktree: run_worktree,
                        },
                        message_id: message_id.clone(),
                    },
                    _ => SessionCommandPayload::Run {
                        request: RunRequest {
                            prompt: content.clone(),
                            provider: resolved.provider.clone().ok_or_else(|| {
                                "Configure a provider before sending".to_string()
                            })?,
                            model: resolved
                                .model
                                .clone()
                                .ok_or_else(|| "Choose a model before sending".to_string())?,
                            reasoning: resolved.reasoning,
                            model_options: resolved.model_options.clone(),
                            cwd,
                            permission_mode,
                            auto_approve: false,
                            attachments: attachment_paths,
                            worktree: run_worktree,
                        },
                        message_id: message_id.clone(),
                    },
                };
                let command = serde_json::to_value(&command)
                    .map_err(|e| format!("Send failed: {e}"))?;
                let mut params = serde_json::json!({ "chatId": chat_id, "command": command });
                if !transfers.is_empty() {
                    params["transfers"] = serde_json::Value::Array(transfers);
                }
                // Every queued command carries a client-minted identity, so
                // the durable-enqueue retry path covers ordinary messages,
                // skill invocations, and Compaction alike.
                if let Some(retry) = retry { params = retry; }
                this.update(cx, |composer, _| {
                    composer.failed_submissions.insert(chat_id.clone(), (submission_text.clone(), params.clone()));
                }).ok();
                // Deadline-bounded: QueueCommand is a local write, but a
                // parked backend handle can stall forever —
                // the send task must never grind silently (2026-08-19).
                attachments::call_with_timeout(
                    &engine,
                    cx.background_executor(),
                    methods::QUEUE_COMMAND,
                    params,
                    std::time::Duration::from_secs(30),
                )
                .await
                .map_err(|e| format!("Send failed: {e}"))?;
                Ok(())
            }
            .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                if result.is_ok() { composer.failed_submissions.remove(&err_chat_id); }
                composer
                    .state
                    .update(cx, |s, _| s.end_upload_progress());
                if let Err(message) = result {
                    // A lost acknowledgement may follow a durable enqueue.
                    // Preserve the chat and retry the original message ID.
                    let restore_key = err_chat_id.clone();
                    composer.failure = Some(message.into());
                    composer.failure_key = Some(restore_key.clone());
                    composer.state.update(cx, |s, cx| {
                        s.remove_echo(&err_chat_id, &err_message_id);
                        s.end_pending_send(&err_chat_id, &err_message_id);
                        if !composer.failed_submissions.contains_key(&restore_key) {
                            for comment in &comments { s.add_diff_comment(&restore_key, comment.clone()); }
                        }
                        cx.notify();
                    });
                    if let Some(restore_text) = restore_text {
                        if composer.current_key != restore_key {
                            composer.drafts.entry(restore_key.clone()).or_insert(restore_text);
                        } else {
                            let fresh = composer.input.read(cx).text().to_string();
                            if fresh.is_empty() {
                                composer.input.update(cx, |input, cx| input.set_text(restore_text, cx));
                            }
                        }
                    }
                    if !staged.is_empty() && !composer.failed_submissions.contains_key(&restore_key) {
                        // Merge by id (stashAttachments): files the user staged
                        // while the send was in flight survive the hand-back —
                        // draining the minted chat's slot too when the restore
                        // target is the canvas.
                        let mut merged = staged.clone();
                        for key in [err_chat_id.clone(), restore_key.clone()] {
                            if let Some(slot) = composer.attachments.get_mut(&key) {
                                let fresh: Vec<_> = slot
                                    .drain(..)
                                    .filter(|e| !merged.iter().any(|f| f.id == e.id))
                                    .collect();
                                merged.extend(fresh);
                            }
                        }
                        composer.attachments.insert(restore_key, merged);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Raw Escape on the composer: interrupt the Turn while a confirm-changes
    /// Approval gates it (ADR-0014, prototype 3-A's "Esc to interrupt"). Surfaces
    /// that own their Escape first — the attachment lightbox, the question
    /// wizard, an open picker popover or switch dialog (which also stops
    /// propagation), and an approval note editor (handled in the transcript)
    /// — so this fires only on a genuinely unclaimed key.
    ///
    /// SCOPE: the handler deliberately lives on the composer root, not the
    /// shell root. A shell-wide hook was evaluated and rejected: bubble-phase
    /// Esc leaks past several surfaces that close themselves without
    /// `stop_propagation` (the rename dialog, the switch dialog) and would
    /// reach inputs that have no Esc semantics of their own (changes-pane
    /// comment drafts), so a global handler would need a growing guard list
    /// to avoid interrupting a run as a side effect of dismissing something.
    /// The composer scope covers the realistic case anyway: the shell lands
    /// initial focus on the composer and re-routes focus there whenever it is
    /// lost (shell.rs `on_focus_lost`), so during a pending approval focus is
    /// in the composer subtree unless a surface that consumes Esc itself
    /// (terminal, dialogs) owns it.
    pub(super) fn on_escape(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key != "escape" {
            return;
        }
        if self.preview.is_some()
            || self.wizard.is_some()
            || self.pickers.read(cx).keyboard_overlay_open()
        {
            return;
        }
        let state = self.state.read(cx);
        if crate::transcript::pending_approval_gate(&state.transcript).is_none() {
            return;
        }
        cx.stop_propagation();
        self.interrupt(cx);
    }

    pub(super) fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let failure_chat = chat_id.clone();
        let params = serde_json::json!({
            "chatId": chat_id,
            "command": { "kind": "interrupt" },
        });
        // Keep each control request alive through delivery, including when
        // Continue follows Stop before its response arrives.
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Stop failed: {err}").into());
                    composer.failure_key = Some(failure_chat);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
    pub(super) fn render_send_button(
        &mut self,
        mode: SendButtonMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::of(cx);
        // Holt composer-actions.tsx: a size-7 filled circle — up-arrow to
        // send/queue, a dark rounded square on the same light circle to stop.
        match mode {
            SendButtonMode::Stop => div()
                .id("composer-stop")
                .role(gpui::Role::Button)
                .aria_label("Stop and pause message queue")
                .focusable()
                .tooltip(|_, cx| {
                    cx.new(|_| super::queue::ActionTooltip("Stop and pause queue"))
                        .into()
                })
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .focus(|s| s.border_2().border_color(theme.border_strong))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.interrupt(cx);
                    }
                }))
                .child(div().size(px(11.0)).rounded(px(3.0)).bg(theme.bg))
                .into_any_element(),
            SendButtonMode::Send | SendButtonMode::Queue => {
                // Dimmed and inert while no project is picked or no agent is
                // runnable (`send_blocked` also gates `on_submit`, so Enter
                // is a no-op too).
                let blocked = self.send_blocked(cx);
                div()
                    .id("composer-send")
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(blocked, |el| el.opacity(0.35))
                    .when(!blocked, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.opacity(0.85))
                            .on_click(cx.listener(|this, _, _, cx| this.on_submit(cx)))
                    })
                    .child(
                        crate::icons::icon(crate::icons::ARROW_UP)
                            .size(px(14.0))
                            .text_color(theme.bg),
                    )
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::slash, failure_restore_text};

    #[test]
    fn compact_failure_does_not_restore_the_command_as_draft() {
        let parsed = slash::parse("/compact");
        assert_eq!(failure_restore_text(&parsed, "/compact".into()), None);
    }

    #[test]
    fn ordinary_send_failure_still_restores_typed_text() {
        let parsed = slash::parse("keep working");
        assert_eq!(
            failure_restore_text(&parsed, "keep working".into()),
            Some("keep working".into())
        );
    }
}
