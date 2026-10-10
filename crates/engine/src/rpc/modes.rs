//! Permission modes and the Plan/Provider mode checkpoints
//! (ADR-0014/0025/0037).

use holt_rpc::{RpcError, RpcReply};
use std::sync::Arc;

use super::{optional_string, required_string};
use crate::EngineService;
use crate::store::persist_chats;

impl EngineService {
    /// Switch a chat's permission mode (ADR-0014): the stored mode is the
    /// single source of truth a Turn snapshots at start, so the switch
    /// takes effect from the next Turn. The choice also becomes the
    /// device's sticky default for new chats. A chat without a config yet
    /// only moves the sticky default — its first run seeds the config from
    /// that default.
    pub(super) fn set_chat_permission_mode(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
    pub(super) fn resolve_approval(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
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
    pub(super) fn enter_plan_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
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
            // ADR-0044: Plan Mode and a goal never overlap (read-only turns
            // could never satisfy the verifier).
            if chat.goal.is_some() {
                return Err(RpcError::BadParams(
                    "the chat has a goal — clear it first (/goal off)".into(),
                ));
            }
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
    pub(super) fn exit_plan_mode(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
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
    pub(super) fn enter_provider_mode(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        // ADR-0044: a goal is durable state with a queue footprint — reject
        // rather than silently clear it (the Plan/Provider swap precedent
        // does not extend here).
        {
            let chats = self
                .runtime
                .chats
                .read()
                .unwrap_or_else(|error| error.into_inner());
            if chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .is_some_and(|chat| chat.goal.is_some())
            {
                return Err(RpcError::BadParams(
                    "the chat has a goal — clear it first (/goal off)".into(),
                ));
            }
        }
        self.exit_plan_mode(serde_json::json!({ "chatId": chat_id }))?;
        self.set_provider_mode(chat_id, true)?;
        RpcReply::value(&self.provider_mode_state(chat_id)?)
    }

    /// Leave Provider Mode (ADR-0037). Idempotent. Pending proposal and key
    /// cards stay writable: they are the user's to settle, not the mode's.
    pub(super) fn exit_provider_mode(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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

    pub(super) fn provider_mode_state(
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
    pub(super) fn resolve_plan_approval(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
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
    pub(super) fn plan_mode_state(
        &self,
        chat_id: &str,
    ) -> Result<holt_proto::PlanModeState, RpcError> {
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

    /// Set or replace the chat's goal (ADR-0044). Planning, provider-mode,
    /// and routine-run chats are rejected (mutual exclusion is rejection,
    /// never a silent clear). The loop starts with the chat's next Turn —
    /// a fresh chat has no captured model settings to build a run from, and
    /// every other harness's `/goal` waits for the next message too.
    pub(super) fn set_goal(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let text = required_string(&params, "text")?.trim().to_string();
        if text.is_empty() {
            return Err(RpcError::BadParams("goal text must not be empty".into()));
        }
        if text.chars().count() > crate::goal::MAX_GOAL_CHARS {
            return Err(RpcError::BadParams(format!(
                "goal text is over the {}-character cap",
                crate::goal::MAX_GOAL_CHARS
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
            if chat.plan_mode.is_some() {
                return Err(RpcError::BadParams(
                    "the chat is in Plan Mode — exit it first (/plan)".into(),
                ));
            }
            if chat.provider_mode {
                return Err(RpcError::BadParams(
                    "the chat is in Provider Mode — exit it first".into(),
                ));
            }
            if chat.routine_run.is_some() {
                return Err(RpcError::BadParams(
                    "a routine run cannot carry a goal".into(),
                ));
            }
            chat.goal = Some(holt_proto::ChatGoalState {
                text: text.clone(),
                status: holt_proto::GoalStatus::Active,
                iteration: 0,
                no_progress: 0,
                eval_failures: 0,
                started_at: chrono::Utc::now(),
                last_reason: None,
            });
            persist_chats(&self.data_dir, &chats)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        self.runtime.publish_chats();
        let chat = self.runtime.chat(chat_id);
        crate::goal::cancel_check(&chat);
        {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue
                .sweep_goal_continuations()
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        }
        RpcReply::value(&self.goal_state(chat_id)?)
    }

    /// Drop the chat's goal (ADR-0044): the row forgets the objective, an
    /// in-flight check is cancelled, and queued continuations are swept —
    /// "off" never leaves a last Turn to run. Idempotent.
    pub(super) fn clear_goal(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let chat = self.runtime.chat(chat_id);
        crate::goal::clear_goal_state(self, &chat);
        RpcReply::value(&self.goal_state(chat_id)?)
    }

    /// Stop or resume the loop while keeping the objective (ADR-0044).
    /// Pausing cancels an in-flight check and sweeps queued continuations.
    /// Resuming grants a fresh budget (the counters reset) and enqueues a
    /// continuation when the chat is idle, so the loop visibly restarts.
    pub(super) fn set_goal_paused(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let paused = params
            .get("paused")
            .and_then(|value| value.as_bool())
            .ok_or_else(|| RpcError::BadParams("paused must be a bool".into()))?;
        let (goal_text, goal_reason) = {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let chat = chats
                .iter_mut()
                .find(|chat| chat.id == chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            let Some(goal) = chat.goal.as_mut() else {
                return Err(RpcError::BadParams("the chat has no goal".into()));
            };
            if paused {
                goal.status = holt_proto::GoalStatus::Paused;
            } else {
                goal.iteration = 0;
                goal.no_progress = 0;
                goal.eval_failures = 0;
                goal.status = holt_proto::GoalStatus::Active;
            }
            let result = (goal.text.clone(), goal.last_reason.clone());
            persist_chats(&self.data_dir, &chats)
                .map_err(|error| RpcError::Failed(error.to_string()))?;
            result
        };
        self.runtime.publish_chats();
        let chat = self.runtime.chat(chat_id);
        crate::goal::cancel_check(&chat);
        if paused {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue
                .sweep_goal_continuations()
                .map_err(|error| RpcError::Failed(error.to_string()))?;
        } else {
            let no_work_queued = {
                let queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                let snapshot = queue.snapshot();
                snapshot.pending.is_empty() && snapshot.active_message_id.is_none()
            };
            if no_work_queued {
                let reason = goal_reason.unwrap_or_else(|| "resumed by the user".into());
                if let Err(error) = crate::goal::enqueue_continuation(
                    self,
                    &chat,
                    crate::goal::continuation_prompt(&goal_text, &reason),
                ) {
                    tracing::warn!(target: "holt::goal", %error, "goal resume could not be queued");
                }
            }
        }
        RpcReply::value(&self.goal_state(chat_id)?)
    }

    /// The chat's goal state as the mutators reply it — the live view
    /// itself rides `WatchChats`.
    fn goal_state(&self, chat_id: &str) -> Result<Option<holt_proto::ChatGoalState>, RpcError> {
        let chats = self
            .runtime
            .chats
            .read()
            .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
        let chat = chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        Ok(chat.goal.clone())
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
