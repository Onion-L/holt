//! The send path: ordinary-message enqueue, direct slash commands,
//! durable-delivery retries, and Stop/interrupt.

use super::Composer;
use super::send_mode::{SendButtonMode, composer_has_content, send_button_mode};

use gpui::{App, Context, div, prelude::*, px};

use holt_doc::SessionCommandPayload;
use holt_proto::RunRequest;
use holt_rpc::methods;

use crate::attachments;
use crate::state::Indicator;
use crate::theme::Theme;

/// The `/plan status` notice line from a `PlanModeState` reply. The
/// proposed plan's own card carries the per-proposal state in the
/// transcript; the notice is just the chat-level mode.
fn plan_status_notice(state: &serde_json::Value) -> String {
    if state["active"] == serde_json::json!(true) {
        "Plan Mode: on — propose a plan with a <proposed_plan> block".into()
    } else {
        "Plan Mode: off".into()
    }
}

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
        if self.pasting.get(&self.current_key).copied().unwrap_or(0) > 0 {
            return true;
        }
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
            self.staged_refs().len(),
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
            // The mode-only /plan forms dispatch themselves (ADR-0025):
            // Enter on the current chat; Off and Status query or clear it.
            // They send no message, so they never reach the send path —
            // a draft chat has nothing to enter or query yet.
            super::slash::Parsed::Plan { action } => match action {
                super::slash::PlanAction::Enter => {
                    self.plan_command("enter", None, cx);
                    return;
                }
                super::slash::PlanAction::Off => {
                    self.plan_command("exit", None, cx);
                    return;
                }
                super::slash::PlanAction::Status => {
                    self.plan_command("status", None, cx);
                    return;
                }
                super::slash::PlanAction::Task(_) => {}
            },
            _ => {}
        }
        let no_content = !composer_has_content(
            &text,
            self.staged_refs().len(),
            self.staged_comments(cx).len(),
        );
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
    fn send(&mut self, mut text: String, cx: &mut Context<Self>) {
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
        // text. Staged image attachments and diff-comment folding stay put
        // for the next ordinary message; path references do NOT travel
        // alone — a skill invocation consumes them into its extra
        // instructions (spec: skills receive both reference forms).
        let mut slash = super::slash::parse(&text);
        // `/plan <task>` (ADR-0025): the task IS the outgoing planning
        // input — an ordinary message — so the content is rewritten here
        // and the directive itself never reaches the model. The enter RPC
        // rides the async block below (after createChat, before the queue);
        // a failed enter restores the ORIGINAL directive (with it), never
        // the bare task — resubmitting that as an ordinary message would
        // silently skip Plan Mode.
        let mut plan_enter = false;
        let restore_text;
        if let super::slash::Parsed::Plan {
            action: super::slash::PlanAction::Task(task),
        } = &slash
        {
            restore_text = Some(text.clone());
            text = task.clone();
            // Normalize the parsed shape so the downstream ordinary-message
            // path (references, stashes, echo) applies untouched.
            slash = super::slash::Parsed::Plain;
            plan_enter = true;
        } else {
            restore_text = None;
        }
        // Both intercepted commands travel alone (ADR-0006/0011): staged
        // attachments and diff-comment folding stay put for the next
        // ordinary message.
        let travels_alone = matches!(
            slash,
            super::slash::Parsed::Skill { .. } | super::slash::Parsed::Compact
        );
        // `typed` keeps the user's own words for the failure hand-back below:
        // restoring the folded prompt would paste the comment block into the
        // input as literal text. Computed now because a retry of an uncertain
        // acknowledgement must leave every stash untouched — the frozen
        // payload already carries those references, and consuming the chips
        // here would drop the newer staging without sending it.
        let typed = restore_text.unwrap_or_else(|| text.clone());
        let retry = self
            .failed_submissions
            .get(&chat_id)
            .filter(|(previous, _)| previous == &typed)
            .map(|(_, params)| params.clone());
        let keeps_stash = travels_alone || retry.is_some();
        // `/compact` (and a retry, per above) leaves path references staged
        // for a later message; ordinary messages and skill invocations
        // consume the chips now.
        let references = if matches!(slash, super::slash::Parsed::Compact) || retry.is_some() {
            Vec::new()
        } else {
            self.path_refs.remove(&self.current_key).unwrap_or_default()
        };
        let key = self.current_key.clone();
        let comments = if keeps_stash {
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
        let submission_text = typed.clone();
        // Inline `@` mention links become readable absolute paths in place
        // BEFORE anything else folds in — the internal `holt-file:` scheme
        // never reaches the queue, the Transcript, or the model.
        let text = super::mentions::resolve_mentions(&text);
        let text = if comments.is_empty() {
            text
        } else {
            crate::comments::with_comments(&text, &comments)
        };
        self.preview = None;
        let message_id = uuid::Uuid::new_v4().to_string();

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
                let content = text.clone();

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
                // `/plan <task>` (ADR-0025): Plan Mode is ON before the run
                // is queued, so the task's Turn admits as a planning Turn
                // (read-only tools + the plan document). Idempotent —
                // entering an already-planning chat is a no-op. FATAL on a
                // new chat, unlike the best-effort createChat above: the
                // engine refuses an unknown chatId, so a createChat failure
                // fails the send instead of silently downgrading to an
                // ordinary implementation message.
                if plan_enter
                    && let Err(err) = attachments::call_with_timeout(
                        &engine,
                        cx.background_executor(),
                        methods::ENTER_PLAN_MODE,
                        serde_json::json!({ "chatId": chat_id }),
                        std::time::Duration::from_secs(30),
                    )
                    .await
                {
                    return Err(format!("/plan failed: {err}"));
                }

                let command = match &slash {
                    // The engine builds the model-visible prompt from the
                    // skill's content; `request.prompt` rides empty. Inline
                    // `@` links in the extra instructions resolve in place
                    // and the attachment-area path list appends to them, so
                    // the skill-prompt construction carries the references.
                    super::slash::Parsed::Skill { name, extra } => {
                        let extra = extra.as_deref().map(super::mentions::resolve_mentions);
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
                            extra_instructions: crate::path_refs::append_references_opt(
                                extra.as_deref(),
                                &references,
                            ),
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
                            // The attachment-area path list appends once, at
                            // the very end of the prompt.
                            prompt: crate::path_refs::append_references(&content, &references),
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
                };
                let command = serde_json::to_value(&command)
                    .map_err(|e| format!("Send failed: {e}"))?;
                let mut params = serde_json::json!({ "chatId": chat_id, "command": command });
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
                    if !references.is_empty() && !composer.failed_submissions.contains_key(&restore_key) {
                        // References staged while the send was in flight
                        // survive the restore, deduped by target path.
                        let mut merged = references.clone();
                        for key in [err_chat_id.clone(), restore_key.clone()] {
                            if let Some(slot) = composer.path_refs.get_mut(&key) {
                                let fresh: Vec<_> = slot
                                    .drain(..)
                                    .filter(|e| !merged.iter().any(|f| f.path == e.path))
                                    .collect();
                                merged.extend(fresh);
                            }
                        }
                        composer.path_refs.insert(restore_key.clone(), merged);
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

    /// The mode-only `/plan` forms (ADR-0025): Enter / Off / Status over
    /// the current chat. They carry no message, so the composer surfaces
    /// the outcome on its notice line and never reaches the send path.
    /// On the new-chat canvas there is nothing to enter or query yet —
    /// `/plan <task>` is the way to start planning there.
    fn plan_command(&mut self, action: &'static str, _task: Option<()>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            self.failure_key = None;
            cx.notify();
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            self.failure = Some("Start a conversation first — or use /plan <task>".into());
            self.failure_key = None;
            cx.notify();
            return;
        };
        let method = match action {
            "enter" => methods::ENTER_PLAN_MODE,
            "exit" => methods::EXIT_PLAN_MODE,
            _ => methods::GET_PLAN_MODE,
        };
        cx.spawn(async move |this, cx| {
            match engine
                .client()
                .call(method, serde_json::json!({ "chatId": chat_id }))
                .await
            {
                Ok(state) => {
                    let notice = match action {
                        "enter" => "Plan Mode on — planning turns are read-only; submit a plan to start implementation".to_string(),
                        "exit" => "Plan Mode off — plan documents are kept".to_string(),
                        _ => plan_status_notice(&state),
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.failure = Some(notice.into());
                        // Chat-scoped like failed sends: chat A's Plan Mode
                        // status must not render under chat B.
                        this.failure_key = Some(chat_id.clone());
                        cx.notify();
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "/plan command failed");
                    let _ = this.update(cx, |this, cx| {
                        this.failure = Some(format!("/plan failed: {err}").into());
                        this.failure_key = Some(chat_id.clone());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
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
                    cx.new(|_| super::queue::ActionTooltip("Stop and pause queue".into()))
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
    use super::{super::slash, failure_restore_text, plan_status_notice};

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

    #[test]
    fn a_failed_task_send_restores_the_full_directive() {
        // The restore hands back the ORIGINAL input — directive included —
        // so a retry re-enters Plan Mode instead of sending the bare task
        // as an ordinary implementation message.
        let parsed = slash::parse("/plan redesign the ingest pipeline");
        assert_eq!(
            failure_restore_text(&parsed, "/plan redesign the ingest pipeline".into()),
            Some("/plan redesign the ingest pipeline".into())
        );
    }

    #[test]
    fn plan_status_notices_render_the_chat_level_mode() {
        assert_eq!(
            plan_status_notice(&serde_json::json!({ "active": false })),
            "Plan Mode: off"
        );
        assert_eq!(
            plan_status_notice(&serde_json::json!({ "active": true })),
            "Plan Mode: on — propose a plan with a <proposed_plan> block"
        );
    }
}
