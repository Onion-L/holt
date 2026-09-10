//! Durable typed-command admission and per-chat serial execution: ordinary
//! messages, skill invocations (each its own Turn), and manual Compaction
//! (the same execution channel, never a Turn — ADR-0011).

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use holt_doc::MessagePart;
use holt_proto::{MessageQueue, PendingKind, PendingMessage, RunRequest, SessionStatus};
use holt_rpc::{RpcError, turns::TurnTerminalEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    EngineService,
    agent::{ChatRuntime, TurnEnd, run_agent_command},
};

/// What one driver iteration produced: a real main-chat Turn's terminal
/// outcome (terminal-event eligible, ADR-0019), or work outside the Turn
/// model — a manual Compaction or a pre-Turn admission failure — as its
/// plain settle pair.
enum DriverOutcome {
    Turn(TurnEnd),
    Settled((bool, Option<String>)),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StartedMessage {
    pub message: PendingMessage,
    pub timestamp: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    pending: Vec<PendingMessage>,
    #[serde(default)]
    priority: Vec<String>,
    #[serde(default)]
    priority_only: bool,
    paused: bool,
    started: Option<StartedMessage>,
    accepted: HashSet<String>,
    /// ADR-0021: the one attended send granted admission through a pause —
    /// a message id bound at first acceptance, served by `head` ahead of
    /// priority, consumed by `start`, and never set for a redelivered id.
    #[serde(default)]
    attended_grant: Option<String>,
}

impl Record {
    /// A settled queue with no remaining work is clean: not paused, no
    /// single-run scope, no outstanding grant. Callers settle the executing
    /// command first and check the queue-level error themselves.
    fn clear_pause_when_empty(&mut self) {
        if self.pending.is_empty() {
            self.paused = false;
            self.priority_only = false;
            self.attended_grant = None;
        }
    }
}

pub(crate) struct Queue {
    record: Record,
    path: PathBuf,
    error: Option<String>,
    unreadable: bool,
    pub tx: watch::Sender<serde_json::Value>,
}

impl Queue {
    pub fn load(data_dir: &Path, chat_id: &str) -> Self {
        let path = data_dir.join("queues").join(format!("{chat_id}.json"));
        let result = if !crate::store::chat_id_is_path_safe(chat_id) {
            Err("invalid chatId".into())
        } else {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<Record>(&bytes).map_err(|e| e.to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
                Err(e) => Err(e.to_string()),
            }
        };
        let (mut record, error) = match result {
            Ok(record) => (record, None),
            Err(error) => (
                Record::default(),
                Some(format!("Could not read the message queue: {error}")),
            ),
        };
        record.paused |= !record.pending.is_empty() || record.started.is_some() || error.is_some();
        if record.started.is_none() && error.is_none() {
            // A saved file's stale pause must not resurrect on an empty,
            // error-free queue (ADR-0021).
            record.clear_pause_when_empty();
        }
        let (tx, _) = watch::channel(serde_json::Value::Null);
        let queue = Self {
            record,
            path,
            unreadable: error.is_some(),
            error,
            tx,
        };
        queue.publish();
        queue
    }

    pub fn recover_started(&mut self) -> Option<StartedMessage> {
        self.record.started.clone()
    }

    pub fn recovered(&mut self, error: Option<String>) {
        if let Some(error) = error {
            self.unreadable = true;
            self.error = Some(format!("Could not recover the interrupted Turn: {error}"));
            self.publish();
            return;
        }
        let mut next = self.record.clone();
        next.started = None;
        if self.error.is_none() {
            next.clear_pause_when_empty();
        }
        if self.commit(next).is_err() {
            self.unreadable = true;
        }
    }

    pub fn snapshot(&self) -> MessageQueue {
        MessageQueue {
            pending: self.record.pending.clone(),
            paused: self.record.paused,
            active_message_id: self
                .record
                .started
                .as_ref()
                .map(|s| s.message.message_id.clone()),
            error: self.error.clone(),
        }
    }

    /// Whether the queue's automatic execution is switched off.
    pub(crate) fn paused(&self) -> bool {
        self.record.paused
    }

    /// Whether no admitted work is executing. Preparation that has not yet
    /// reached the admission checkpoint still occupies the channel through
    /// the driver, so a caller granting attended admission must also check
    /// `ChatRuntime::driver_running`.
    pub(crate) fn idle(&self) -> bool {
        self.record.started.is_none() && !self.unreadable
    }

    fn publish(&self) {
        self.tx
            .send_replace(serde_json::to_value(self.snapshot()).expect("queue snapshot"));
    }

    fn commit(&mut self, record: Record) -> Result<(), RpcError> {
        if self.unreadable {
            return Err(RpcError::Failed(self.error.clone().unwrap_or_default()));
        }
        let mut renamed = false;
        let mut write = || -> Result<(), Box<dyn std::error::Error>> {
            let parent = self.path.parent().expect("queue directory");
            std::fs::create_dir_all(parent)?;
            let temp = self
                .path
                .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp)?;
                file.write_all(&serde_json::to_vec(&record)?)?;
                file.sync_all()?;
                std::fs::rename(&temp, &self.path)?;
                renamed = true;
                std::fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            let _ = std::fs::remove_file(temp);
            result
        };
        if let Err(error) = write() {
            // After rename the durable outcome is uncertain. Keep the new
            // identities, but require a successful save before executing.
            if renamed {
                self.record = record;
                self.record.paused = true;
            }
            self.error = Some(format!("Could not save the message queue: {error}"));
            self.publish();
            return Err(RpcError::Failed(self.error.clone().unwrap()));
        }
        self.record = record;
        self.error = None;
        self.publish();
        Ok(())
    }

    pub fn enqueue(
        &mut self,
        request: RunRequest,
        message_id: String,
        kind: PendingKind,
        skill_name: Option<String>,
        extra_instructions: Option<String>,
        attended: bool,
    ) -> Result<(), RpcError> {
        if self.record.accepted.contains(&message_id) {
            return if self.error.is_some() {
                self.commit(self.record.clone())
            } else {
                Ok(())
            };
        }
        let mut next = self.record.clone();
        next.accepted.insert(message_id.clone());
        next.pending.push(PendingMessage {
            message_id: message_id.clone(),
            request,
            kind,
            skill_name,
            extra_instructions,
            submitted_at: chrono::Utc::now().timestamp_millis(),
            error: None,
        });
        // An attended send is granted only on first acceptance and only
        // through a pause: the grant, the unpause, and the solo scope are
        // one durable change, so an enqueue can never be half-granted.
        if attended && self.record.paused {
            next.attended_grant = Some(message_id);
            next.paused = false;
            next.priority_only = true;
        }
        self.commit(next)
    }

    pub fn enqueue_priority(
        &mut self,
        request: RunRequest,
        message_id: String,
    ) -> Result<(), RpcError> {
        self.enqueue(
            request,
            message_id.clone(),
            PendingKind::Ordinary,
            None,
            None,
            false,
        )?;
        let mut next = self.record.clone();
        next.priority.retain(|id| id != &message_id);
        next.priority.push(message_id);
        self.commit(next)
    }

    /// How edit/delete must answer for an id that is not waiting in the
    /// queue: a started item has already become a Turn, anything else was
    /// never (or is no longer) pending.
    fn mutation_refusal(&self, message_id: &str) -> RpcError {
        let started = self
            .record
            .started
            .as_ref()
            .is_some_and(|s| s.message.message_id == message_id);
        RpcError::Failed(if started {
            "Message is already executing".into()
        } else {
            "Message is no longer pending".into()
        })
    }

    /// Change the one editable field of a pending item: an ordinary
    /// message's body, or a skill invocation's extra instructions. Identity,
    /// position, kind, and the captured model settings are the queue's — an
    /// item that already started is no longer editable, and a pending manual
    /// Compaction has no editable field at all (delete and resubmit instead).
    pub fn edit(&mut self, message_id: &str, prompt: String) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        let Some(item) = next
            .pending
            .iter_mut()
            .find(|item| item.message_id == message_id)
        else {
            return Err(self.mutation_refusal(message_id));
        };
        match item.kind {
            PendingKind::Ordinary => {
                if prompt.trim().is_empty() {
                    return Err(RpcError::BadParams("prompt must not be empty".into()));
                }
                item.request.prompt = prompt;
            }
            PendingKind::Skill => {
                item.extra_instructions = (!prompt.trim().is_empty()).then_some(prompt);
            }
            PendingKind::Compact => {
                return Err(RpcError::Failed(
                    "A queued Compaction cannot be edited — delete it and submit again".into(),
                ));
            }
        }
        self.commit(next)
    }

    /// Remove a pending item. The others keep their relative order; a started
    /// item is execution's property now and is never removed here.
    pub fn delete(&mut self, message_id: &str) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        let Some(index) = next
            .pending
            .iter()
            .position(|item| item.message_id == message_id)
        else {
            return Err(self.mutation_refusal(message_id));
        };
        next.pending.remove(index);
        next.priority.retain(|id| id != message_id);
        if next.attended_grant.as_deref() == Some(message_id) {
            next.attended_grant = None;
        }
        if next.pending.is_empty() && next.started.is_none() && self.error.is_none() {
            next.clear_pause_when_empty();
        }
        self.commit(next)
    }

    pub fn pause(&mut self, paused: bool) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        next.paused = paused;
        let result = self.commit(next);
        if result.is_err() && paused {
            self.record.paused = true;
            self.publish();
        }
        result
    }

    /// Continue: resume automatic execution and lift the single-run scope
    /// of an outstanding attended send in one durable change. The grant
    /// itself stays — past this point it is ordering only.
    pub(crate) fn resume(&mut self) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        next.paused = false;
        next.priority_only = false;
        self.commit(next)
    }

    fn head(&self) -> Option<PendingMessage> {
        let by_id =
            |record: &Record, id: &str| record.pending.iter().find(|m| m.message_id == id).cloned();
        (!self.record.paused && !self.unreadable)
            .then(|| {
                // The attended grant outranks everything: it exists precisely
                // to jump a promoted item parked ahead of the pause.
                self.record
                    .attended_grant
                    .as_deref()
                    .and_then(|id| by_id(&self.record, id))
                    .or_else(|| {
                        self.record
                            .priority
                            .iter()
                            .find_map(|id| by_id(&self.record, id))
                    })
                    .or_else(|| self.record.pending.first().cloned())
            })
            .flatten()
    }

    /// Move a pending item into the priority order (Run now / Steer of an
    /// existing item). Manual Compaction has no Run now action: it executes
    /// strictly in submission order.
    pub fn promote(&mut self, message_id: &str) -> Result<(), RpcError> {
        let Some(item) = self
            .record
            .pending
            .iter()
            .find(|m| m.message_id == message_id)
        else {
            return Err(self.mutation_refusal(message_id));
        };
        if item.kind == PendingKind::Compact {
            return Err(RpcError::Failed(
                "A queued Compaction executes in order and cannot run now".into(),
            ));
        }
        let mut next = self.record.clone();
        // A Run now on a paused queue keeps the rest parked; a promotion
        // landing mid-attended-run must not downgrade the outstanding solo
        // scope either (ADR-0021).
        next.priority_only = next.paused || next.priority_only;
        next.priority.retain(|id| id != message_id);
        next.priority.push(message_id.to_string());
        self.commit(next)
    }

    /// The admission checkpoint: atomically move the head from pending to
    /// started and persist it before any model or tool work. Returns the
    /// admitted item, so the caller builds its Turn from the body the queue
    /// holds NOW — an edit that landed between the queue pick and this
    /// checkpoint wins.
    pub fn start(&mut self, message_id: &str, timestamp: i64) -> Result<StartedMessage, RpcError> {
        if self.record.paused {
            return Err(RpcError::Failed("Message queue is paused".into()));
        }
        let mut next = self.record.clone();
        let is_head = next.attended_grant.as_deref() == Some(message_id)
            || next.priority.first().map_or_else(
                || {
                    next.pending
                        .first()
                        .is_some_and(|m| m.message_id == message_id)
                },
                |id| id == message_id,
            );
        if !is_head {
            return Err(RpcError::Failed("Message is no longer pending".into()));
        }
        let index = next
            .pending
            .iter()
            .position(|m| m.message_id == message_id)
            .expect("head exists");
        let started = StartedMessage {
            message: next.pending.remove(index),
            timestamp,
        };
        next.priority.retain(|id| id != message_id);
        if next.attended_grant.as_deref() == Some(message_id) {
            // Single-run: the grant is spent at admission.
            next.attended_grant = None;
        }
        next.started = Some(started.clone());
        let result = self.commit(next);
        if result.is_err() && self.record.started.is_some() {
            // The rename landed but its directory sync did not. Recovery
            // must settle this checkpoint before another item can start.
            self.unreadable = true;
            self.record.paused = true;
            self.error = Some("Turn admission could not be confirmed. Restore storage and reopen Holt to recover the checkpoint.".into());
            self.publish();
        }
        result.map(|()| started)
    }

    /// Settle the active work. The returned Result reports whether the
    /// completion was durably recorded: the queue/session error behavior on
    /// a persistence failure is unchanged, but callers publishing downstream
    /// signals (the Turn terminal event, ADR-0019) must only fire after a
    /// successful commit.
    fn finish(&mut self, success: bool, error: Option<String>) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        let was_started = next.started.take().is_some();
        next.paused |= !success || next.priority_only;
        if was_started && next.priority_only {
            next.priority_only = false;
        }
        if !was_started && let Some(head) = next.pending.first_mut() {
            head.error = error.clone();
        }
        // The clean-state invariant outranks the failure pause: with no
        // work left and no queue-level error (this settle must not be one),
        // there is nothing for a pause to protect (ADR-0021).
        if self.error.is_none() && error.is_none() {
            next.clear_pause_when_empty();
        }
        if let Err(commit_error) = self.commit(next) {
            self.record.paused = true;
            self.unreadable |= self.record.started.is_some();
            self.error = Some(commit_error.to_string());
            self.publish();
            Err(commit_error)
        } else {
            if was_started && let Some(error) = error {
                self.error = Some(error);
                self.publish();
            }
            Ok(())
        }
    }
}

impl EngineService {
    /// The queued manual Compaction (ADR-0011): never a Turn. Model and
    /// credential failures retain the head before admission. Nothing to
    /// compact settles as a notice without a model request or queue pause.
    /// The checkpoint guarantees a restart never retries a Compaction that
    /// already began. Execution
    /// publishes `Compacting`, swaps the History only on success, and
    /// touches nothing Turn-scoped — no baseline, no transcript echo.
    async fn run_queued_compaction(
        &self,
        chat: &Arc<ChatRuntime>,
        message: &PendingMessage,
        cancel: &CancellationToken,
    ) -> (bool, Option<String>) {
        let request = &message.request;
        let history = chat
            .history
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !crate::compaction::has_compactable_content(&history) {
            // Admit even a no-op so completion and delivery deduplication
            // follow the same durable checkpoint as a real Compaction.
            let admitted = {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() || chat.is_removed() {
                    return (false, None);
                }
                queue.start(&message.message_id, chrono::Utc::now().timestamp_millis())
            };
            if let Err(error) = admitted {
                return (false, (!cancel.is_cancelled()).then(|| error.to_string()));
            }
            crate::agent::push_system_part(
                chat,
                &self.engine_info.device_id,
                format!("compaction-skipped-{}", message.message_id),
                MessagePart::Notice {
                    id: "n0".into(),
                    message: "There is nothing to compact".into(),
                },
            );
            return (true, None);
        }
        let model = match self
            .providers
            .resolve_model(request.provider.as_str(), &request.model)
        {
            Ok(model) => model,
            Err(error) => return (false, Some(error.to_string())),
        };
        let Some(api_key) = self
            .providers
            .credentials
            .reveal_key(request.provider.as_str())
            .await
        else {
            return (
                false,
                Some(format!("provider {} is not configured", request.provider)),
            );
        };
        if cancel.is_cancelled() || chat.is_removed() {
            return (false, None);
        }
        let admitted = {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() || chat.is_removed() {
                return (false, None);
            }
            queue.start(&message.message_id, chrono::Utc::now().timestamp_millis())
        };
        if let Err(error) = admitted {
            return (false, (!cancel.is_cancelled()).then(|| error.to_string()));
        }
        let stream_fn = self
            .runtime
            .stream_fn
            .clone()
            .unwrap_or_else(crate::agent::default_stream_fn);
        self.runtime
            .set_session(&chat.chat_id, SessionStatus::Compacting);
        let outcome = crate::compaction::compact_now(
            &history,
            &model,
            &stream_fn,
            &api_key,
            holt_doc::parts::CompactionTrigger::Manual,
            Some(cancel),
        )
        .await;
        match outcome {
            Ok(Some(outcome)) => {
                crate::agent::record_turn_start_compaction(
                    chat,
                    &self.engine_info.device_id,
                    &outcome.record,
                );
                *chat.history.write().unwrap_or_else(|e| e.into_inner()) = outcome.messages;
                // A manual compaction pays the overflow debt too.
                self.runtime.take_compact_before_next_turn(&chat.chat_id);
                (true, None)
            }
            // Pre-checked above; losing the race just settles.
            Ok(None) => (true, None),
            Err(reason) => {
                // An interruption is the user's own act — settle quietly. A
                // real failure surfaces on the Transcript; the History is
                // untouched either way.
                if cancel.is_cancelled() {
                    (false, None)
                } else {
                    tracing::warn!(target: "holt::compaction", %reason, "manual compaction failed");
                    crate::agent::push_system_part(
                        chat,
                        &self.engine_info.device_id,
                        format!("compaction-failed-{}", uuid::Uuid::new_v4()),
                        MessagePart::Notice {
                            id: "n0".into(),
                            message: format!(
                                "Compaction failed ({reason}); the conversation was \
                                 left unchanged."
                            ),
                        },
                    );
                    (false, Some(format!("Compaction failed ({reason})")))
                }
            }
        }
    }

    pub(crate) fn kick_queue(&self, chat: Arc<ChatRuntime>) {
        if chat.driver_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let service = self.clone();
        let worker_chat = chat.clone();
        let task = tokio::spawn(async move {
            loop {
                let _execution = worker_chat.execution.lock().await;
                let (message, cancel, picked_id) = {
                    let queue = worker_chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                    let head = if worker_chat.is_removed() {
                        None
                    } else {
                        queue.head()
                    };
                    let Some(message) = head else {
                        worker_chat.driver_running.store(false, Ordering::Release);
                        return;
                    };
                    let cancel = CancellationToken::new();
                    *worker_chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(cancel.clone());
                    let picked_id = message.message_id.clone();
                    (message, cancel, picked_id)
                };
                let kind = message.kind;
                let heartbeat_stop = CancellationToken::new();
                let heartbeat = tokio::spawn(crate::agent::heartbeat_session(
                    service.runtime.clone(),
                    worker_chat.chat_id.clone(),
                    heartbeat_stop.clone(),
                ));
                worker_chat.track_task(&heartbeat);
                let outcome = match kind {
                    PendingKind::Compact => DriverOutcome::Settled(
                        service
                            .run_queued_compaction(&worker_chat, &message, &cancel)
                            .await,
                    ),
                    PendingKind::Ordinary | PendingKind::Skill => {
                        // A queued skill resolves against a fresh catalog at
                        // admission (rule 16): an unknown or invalid name
                        // retains the pending item with an error and pauses
                        // the queue BEFORE any Turn is created.
                        let skill = if kind == PendingKind::Skill {
                            let name = message.skill_name.clone().unwrap_or_default();
                            match service
                                .skills
                                .resolve(Some(&message.request.cwd), &name)
                                .await
                            {
                                Some(skill) => Ok(Some(skill)),
                                // Cancel-aware like the start_turn error path
                                // below: a Steer/Stop that landed during the
                                // scan settles quietly.
                                None if cancel.is_cancelled() => Err((false, None)),
                                None => Err((false, Some(format!("unknown skill: {name}")))),
                            }
                        } else {
                            Ok(None)
                        };
                        match skill {
                            Err(outcome) => DriverOutcome::Settled(outcome),
                            Ok(skill) => {
                                let prompt = message.request.prompt.clone();
                                let prepared = service
                                    .start_turn(
                                        &worker_chat.chat_id,
                                        worker_chat.clone(),
                                        message.request,
                                        message.message_id,
                                        vec![MessagePart::Text {
                                            id: "t0".into(),
                                            text: prompt.clone(),
                                        }],
                                        prompt.clone(),
                                        prompt.clone(),
                                        None,
                                        Some(prompt),
                                        cancel.clone(),
                                        true,
                                        skill,
                                    )
                                    .await;
                                match prepared {
                                    Ok(run) => DriverOutcome::Turn(run_agent_command(run).await),
                                    Err(error) => DriverOutcome::Settled((
                                        false,
                                        (!cancel.is_cancelled()).then(|| error.to_string()),
                                    )),
                                }
                            }
                        }
                    }
                };
                // A failed Turn pauses the queue without a queue-level error,
                // as before — its reason rides only the terminal event's
                // internal diagnostic field.
                let (success, error, turn_end) = match outcome {
                    DriverOutcome::Turn(end) => (
                        matches!(end, crate::agent::TurnEnd::Succeeded),
                        None,
                        Some(end),
                    ),
                    DriverOutcome::Settled((success, error)) => (success, error, None),
                };
                // Wait for the heartbeat to stop before publishing the final
                // status so a last tick cannot revive an idle session.
                heartbeat_stop.cancel();
                let _ = heartbeat.await;
                let started = worker_chat
                    .queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record
                    .started
                    .is_some();
                let persistence_error = worker_chat
                    .persistence_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let mut queue = worker_chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                *worker_chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                // The iteration settles the queue only if the picked head
                // reached its admission checkpoint. A delete that removed it
                // mid-prep leaves no run to finish: keep the winning
                // mutation's state and consider the next head directly.
                let vanished = !started
                    && queue
                        .record
                        .pending
                        .iter()
                        .all(|m| m.message_id != picked_id);
                let settled = if !vanished && !worker_chat.is_removed() && !queue.unreadable {
                    // Stop already changed the pause state. A subsequent
                    // Continue must survive the canceled Turn's cleanup.
                    Some(queue.finish(
                        (success || cancel.is_cancelled()) && persistence_error.is_none(),
                        persistence_error.clone().or(error),
                    ))
                } else {
                    None
                };
                if started {
                    // A failed or interrupted Compaction settles Idle like a
                    // successful one: it was never a Turn, so there is no
                    // errored Turn to report — the pause and the Transcript
                    // notice carry the failure.
                    service.runtime.set_session(
                        &worker_chat.chat_id,
                        if kind == PendingKind::Compact || success || cancel.is_cancelled() {
                            SessionStatus::Idle
                        } else {
                            SessionStatus::Errored
                        },
                    );
                }
                // The Turn terminal event (ADR-0019): exactly one per real
                // main-chat Turn, only AFTER Transcript and History settled
                // and queue completion was durably recorded. A completion
                // that could not be persisted keeps the queue/session error
                // and emits nothing; failed conversation writes likewise
                // leave the durable prerequisite false. Publishing is
                // fire-and-forget — a closed or lagging consumer changes
                // nothing about the settled Turn or the next queued item.
                if let Some(end) = turn_end
                    && matches!(settled, Some(Ok(())))
                    && persistence_error.is_none()
                {
                    let (outcome, reason) = match end {
                        TurnEnd::Succeeded => (holt_rpc::turns::TurnOutcome::Succeeded, None),
                        TurnEnd::Failed { reason } => {
                            (holt_rpc::turns::TurnOutcome::Failed, Some(reason))
                        }
                        TurnEnd::Interrupted => (holt_rpc::turns::TurnOutcome::Interrupted, None),
                    };
                    service.turn_events.publish(TurnTerminalEvent {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        chat_id: worker_chat.chat_id.clone(),
                        message_id: picked_id,
                        outcome,
                        finished_at: chrono::Utc::now().timestamp_millis(),
                        internal_reason: reason,
                    });
                }
            }
        });
        chat.track_task(&task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(prompt: &str, model: &str) -> RunRequest {
        serde_json::from_value(serde_json::json!({
            "prompt": prompt,
            "provider": "openai",
            "model": model,
            "reasoning": "high",
            "cwd": "/tmp/project",
        }))
        .expect("run request")
    }

    fn enqueue_ordinary(queue: &mut Queue, prompt: &str, model: &str, message_id: &str) {
        queue
            .enqueue(
                request(prompt, model),
                message_id.into(),
                PendingKind::Ordinary,
                None,
                None,
                false,
            )
            .expect("enqueue");
    }

    fn queue_with_pending() -> (Queue, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut queue = Queue::load(dir.path(), "chat-1");
        enqueue_ordinary(&mut queue, "B", "openai/gpt-5.4", "m-b");
        enqueue_ordinary(&mut queue, "C", "openai/gpt-5.4-mini", "m-c");
        (queue, dir)
    }

    #[test]
    fn edit_changes_only_the_body() {
        let (mut queue, _dir) = queue_with_pending();
        let before = queue.record.pending[0].clone();
        queue.edit("m-b", "B edited".into()).expect("edit");
        let after = queue.record.pending[0].clone();
        assert_eq!(after.request.prompt, "B edited");
        assert_eq!(after.message_id, before.message_id);
        assert_eq!(after.submitted_at, before.submitted_at);
        assert_eq!(after.request.model, before.request.model);
        assert_eq!(after.request.provider, before.request.provider);
        assert_eq!(after.request.reasoning, before.request.reasoning);
        assert_eq!(after.request.model_options, before.request.model_options);
        assert_eq!(queue.record.pending.len(), 2);
        assert_eq!(queue.record.pending[1].request.prompt, "C");
    }

    #[test]
    fn delete_removes_only_the_named_item_and_keeps_order() {
        let (mut queue, _dir) = queue_with_pending();
        queue.delete("m-b").expect("delete");
        assert_eq!(
            queue
                .record
                .pending
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m-c"]
        );
        // Deleting the tail works the same way.
        enqueue_ordinary(&mut queue, "D", "openai/gpt-5.4", "m-d");
        queue.delete("m-c").expect("delete tail");
        assert_eq!(
            queue
                .record
                .pending
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m-d"]
        );
    }

    #[test]
    fn deleting_the_last_pending_item_clears_pause_and_priority() {
        let (mut queue, dir) = queue_with_pending();
        queue.pause(true).unwrap();
        queue.delete("m-b").unwrap();
        assert!(queue.snapshot().paused, "remaining work stays paused");
        queue.promote("m-c").unwrap();
        queue.delete("m-c").unwrap();
        assert!(!queue.snapshot().paused);
        assert!(queue.record.priority.is_empty());
        assert!(!queue.record.priority_only);
        assert!(!Queue::load(dir.path(), "chat-1").snapshot().paused);
        enqueue_ordinary(&mut queue, "hi", "openai/gpt-5.4", "m-hi");
        queue.start("m-hi", 1).unwrap();
        queue.finish(true, None).unwrap();
        assert!(
            !queue.snapshot().paused,
            "a deleted priority item must not re-pause the queue"
        );
    }

    #[test]
    fn deleting_the_last_pending_item_preserves_pause_during_execution() {
        let (mut queue, _dir) = queue_with_pending();
        queue.start("m-b", 1).unwrap();
        queue.pause(true).unwrap();
        queue.delete("m-c").unwrap();
        assert!(queue.snapshot().paused);
        assert_eq!(queue.snapshot().active_message_id.as_deref(), Some("m-b"));
        queue.finish(true, None).unwrap();
        // The delete itself must not lift the pause while m-b executes, but
        // once it settles with no work left and no queue-level error, the
        // clean-state invariant clears the pause (ADR-0021).
        assert!(!queue.snapshot().paused);
    }

    #[test]
    fn deleting_the_last_pending_item_preserves_pause_after_a_queue_error() {
        let (mut queue, _dir) = queue_with_pending();
        queue.pause(true).unwrap();
        queue.delete("m-b").unwrap();
        queue.error = Some("Could not save the message queue".into());
        queue.delete("m-c").unwrap();
        assert!(queue.snapshot().paused);
    }

    #[test]
    fn an_attended_send_runs_first_through_a_paused_queue_and_leaves_it_paused() {
        let (mut queue, _dir) = queue_with_pending();
        queue.pause(true).unwrap();
        queue.promote("m-b").unwrap();
        assert!(queue.snapshot().paused);

        // The user's fresh send is granted admission even though the queue
        // is paused: it runs ahead of the promoted m-b, and the queue
        // returns to its pause when it settles (ADR-0021).
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        assert!(!queue.snapshot().paused, "the grant unblocks execution");
        assert_eq!(queue.snapshot().active_message_id, None);

        let started = queue
            .start("m-fresh", 1)
            .expect("the grant runs ahead of parked priority");
        assert_eq!(started.message.request.prompt, "fresh");
        queue.finish(true, None).expect("settle");

        assert!(queue.snapshot().paused, "the solo run re-pauses the queue");
        assert_eq!(
            queue
                .snapshot()
                .pending
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m-b", "m-c"],
            "parked items keep their order"
        );
        queue
            .start("m-b", 2)
            .expect_err("the pause still holds for parked items");
    }

    #[test]
    fn a_redelivered_pending_message_does_not_gain_a_grant() {
        let (mut queue, _dir) = queue_with_pending();
        queue.pause(true).unwrap();
        // A durable-delivery retry replays an already-accepted command
        // while the queue is parked: dedup answers Ok and grants nothing.
        queue
            .enqueue(
                request("C", "openai/gpt-5.4"),
                "m-c".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("redelivery");
        assert!(queue.snapshot().paused, "the retry must not unpause");
        queue
            .start("m-c", 1)
            .expect_err("a redelivered id is never an admission");
    }

    #[test]
    fn deleting_the_granted_message_revokes_the_grant() {
        let (mut queue, _dir) = queue_with_pending();
        queue.pause(true).unwrap();
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        queue.delete("m-fresh").expect("delete the granted message");
        // With the grant gone the queue serves its normal head again.
        let started = queue.start("m-b", 1).expect("normal head is runnable");
        assert_eq!(started.message.message_id, "m-b");
    }

    #[test]
    fn a_failed_settle_with_no_work_left_restores_a_clean_queue() {
        let (mut queue, _dir) = queue_with_pending();
        queue.delete("m-b").unwrap();
        queue.delete("m-c").unwrap();
        queue.pause(true).unwrap();
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        queue.start("m-fresh", 1).unwrap();
        queue.finish(false, None).expect("failed settle");
        assert!(
            !queue.snapshot().paused,
            "nothing left to protect: the clean-state invariant outranks the failure pause"
        );
    }

    #[test]
    fn a_queue_level_error_keeps_the_pause_on_an_empty_settle() {
        let (mut queue, _dir) = queue_with_pending();
        queue.delete("m-b").unwrap();
        queue.delete("m-c").unwrap();
        queue.pause(true).unwrap();
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        queue.start("m-fresh", 1).unwrap();
        queue.error = Some("Could not save the message queue".into());
        queue.finish(true, None).expect("settle");
        assert!(
            queue.snapshot().paused,
            "a queue-level error keeps the pause"
        );
    }

    #[test]
    fn a_saved_clean_queue_reloads_unpaused() {
        let (mut queue, dir) = queue_with_pending();
        queue.delete("m-b").unwrap();
        queue.delete("m-c").unwrap();
        // Residue: a pause persisted over an empty, error-free queue.
        queue.pause(true).unwrap();
        let reloaded = Queue::load(dir.path(), "chat-1");
        assert!(
            !reloaded.snapshot().paused,
            "a stale saved pause must not resurrect on a clean queue"
        );
    }

    #[test]
    fn recovering_a_started_item_with_no_work_left_restores_a_clean_queue() {
        let (mut queue, _dir) = queue_with_pending();
        queue.delete("m-b").unwrap();
        queue.delete("m-c").unwrap();
        queue.pause(true).unwrap();
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        queue.start("m-fresh", 1).unwrap();
        queue.pause(true).expect("Stop during the attended run");
        queue.recovered(None);
        assert!(
            !queue.snapshot().paused,
            "the recovered queue is settled, empty, and error-free: clean state"
        );
    }

    #[test]
    fn promoting_during_an_attended_run_preserves_the_solo_scope() {
        let (mut queue, _dir) = queue_with_pending();
        queue.pause(true).unwrap();
        queue
            .enqueue(
                request("fresh", "openai/gpt-5.4"),
                "m-fresh".into(),
                PendingKind::Ordinary,
                None,
                None,
                true,
            )
            .expect("attended enqueue");
        queue.start("m-fresh", 1).expect("the grant runs");
        assert!(!queue.snapshot().paused);

        // A Run now / Steer landing mid-attended-run must not downgrade the
        // single-run scope: when the interrupted-or-promoted work settles,
        // the queue still returns to its pause (ADR-0021).
        queue.promote("m-b").expect("promote during the run");
        queue.finish(true, None).expect("settle");
        assert!(
            queue.snapshot().paused,
            "the solo scope survives a mid-run promotion"
        );
        assert_eq!(
            queue
                .snapshot()
                .pending
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m-b", "m-c"],
        );
    }

    #[test]
    fn start_hands_back_the_currently_stored_body() {
        let (mut queue, _dir) = queue_with_pending();
        queue.edit("m-b", "B edited".into()).expect("edit");
        let started = queue.start("m-b", 42).expect("start");
        assert_eq!(started.message.request.prompt, "B edited");
        assert_eq!(started.timestamp, 42);
        assert_eq!(started.message.message_id, "m-b");
        assert!(
            queue
                .record
                .pending
                .first()
                .is_some_and(|m| m.message_id == "m-c")
        );
    }

    #[test]
    fn mutations_answer_for_started_and_unknown_ids() {
        let (mut queue, _dir) = queue_with_pending();
        queue.start("m-b", 1).expect("start");
        let error = queue.edit("m-b", "nope".into()).unwrap_err();
        assert!(error.to_string().contains("already executing"), "{error}");
        let error = queue.delete("m-b").unwrap_err();
        assert!(error.to_string().contains("already executing"), "{error}");
        let error = queue.edit("m-zz", "nope".into()).unwrap_err();
        assert!(error.to_string().contains("no longer pending"), "{error}");
        let error = queue.delete("m-zz").unwrap_err();
        assert!(error.to_string().contains("no longer pending"), "{error}");
    }

    #[test]
    fn edits_and_deletions_survive_a_reload() {
        let (mut queue, dir) = queue_with_pending();
        queue.edit("m-c", "C edited".into()).expect("edit");
        queue.delete("m-b").expect("delete");
        let reloaded = Queue::load(dir.path(), "chat-1");
        assert_eq!(reloaded.record.pending.len(), 1);
        assert_eq!(reloaded.record.pending[0].message_id, "m-c");
        assert_eq!(reloaded.record.pending[0].request.prompt, "C edited");
        assert_eq!(
            reloaded.record.pending[0].request.model,
            "openai/gpt-5.4-mini"
        );
        assert!(reloaded.record.paused, "a non-empty queue restores paused");
    }

    fn queue_with_typed() -> (Queue, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut queue = Queue::load(dir.path(), "chat-1");
        queue
            .enqueue(
                request("", "openai/gpt-5.4"),
                "m-skill".into(),
                PendingKind::Skill,
                Some("grill".into()),
                Some("focus on the data layer".into()),
                false,
            )
            .expect("enqueue skill");
        queue
            .enqueue(
                request("", "openai/gpt-5.4"),
                "m-compact".into(),
                PendingKind::Compact,
                None,
                None,
                false,
            )
            .expect("enqueue compact");
        (queue, dir)
    }

    #[test]
    fn a_skill_edit_changes_only_the_extra_instructions() {
        let (mut queue, _dir) = queue_with_typed();
        let before = queue.record.pending[0].clone();
        queue
            .edit("m-skill", "different focus".into())
            .expect("edit skill");
        let after = &queue.record.pending[0];
        assert_eq!(after.extra_instructions.as_deref(), Some("different focus"));
        assert_eq!(after.kind, PendingKind::Skill);
        assert_eq!(after.skill_name.as_deref(), Some("grill"));
        assert_eq!(after.message_id, before.message_id);
        assert_eq!(after.submitted_at, before.submitted_at);
        assert_eq!(after.request.model, before.request.model);
        // Clearing the extra instructions is a valid edit.
        queue.edit("m-skill", "  ".into()).expect("clear extra");
        assert_eq!(queue.record.pending[0].extra_instructions, None);
        // The prompt stays the queue's, never the editor's.
        assert_eq!(queue.record.pending[0].request.prompt, "");
    }

    #[test]
    fn an_ordinary_edit_still_requires_a_body() {
        let (mut queue, _dir) = queue_with_pending();
        assert!(queue.edit("m-b", "   ".into()).is_err());
        assert_eq!(queue.record.pending[0].request.prompt, "B");
    }

    #[test]
    fn a_compaction_has_no_edit_or_run_now_but_deletes() {
        let (mut queue, _dir) = queue_with_typed();
        let error = queue.edit("m-compact", "nope".into()).unwrap_err();
        assert!(error.to_string().contains("cannot be edited"), "{error}");
        let error = queue.promote("m-compact").unwrap_err();
        assert!(error.to_string().contains("cannot run now"), "{error}");
        // The skill ahead of it keeps its spot; the compaction deletes fine.
        queue.delete("m-compact").expect("delete compact");
        assert_eq!(queue.record.pending.len(), 1);
        assert_eq!(queue.record.pending[0].message_id, "m-skill");
    }

    #[test]
    fn a_skill_promotes_into_the_priority_order() {
        let (mut queue, _dir) = queue_with_typed();
        queue.promote("m-skill").expect("promote skill");
        assert_eq!(queue.record.priority, vec!["m-skill".to_string()]);
    }

    #[test]
    fn typed_items_survive_a_reload() {
        let (mut queue, dir) = queue_with_typed();
        queue
            .edit("m-skill", "edited extra".into())
            .expect("edit skill");
        let reloaded = Queue::load(dir.path(), "chat-1");
        assert_eq!(reloaded.record.pending.len(), 2);
        let skill = &reloaded.record.pending[0];
        assert_eq!(skill.kind, PendingKind::Skill);
        assert_eq!(skill.skill_name.as_deref(), Some("grill"));
        assert_eq!(skill.extra_instructions.as_deref(), Some("edited extra"));
        assert_eq!(reloaded.record.pending[1].kind, PendingKind::Compact);
        assert!(reloaded.record.paused, "a non-empty queue restores paused");
    }
}
