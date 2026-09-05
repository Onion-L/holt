//! Durable ordinary-message admission and per-chat serial execution.

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use holt_doc::MessagePart;
use holt_proto::{MessageQueue, PendingMessage, RunRequest};
use holt_rpc::RpcError;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    EngineService,
    agent::{ChatRuntime, run_agent_command},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StartedMessage {
    pub message: PendingMessage,
    pub timestamp: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    pending: Vec<PendingMessage>,
    paused: bool,
    started: Option<StartedMessage>,
    accepted: HashSet<String>,
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

    pub fn enqueue(&mut self, request: RunRequest, message_id: String) -> Result<(), RpcError> {
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
            message_id,
            request,
            submitted_at: chrono::Utc::now().timestamp_millis(),
            error: None,
        });
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

    /// Change a pending ordinary message's body. Identity, position, and the
    /// captured model settings are the queue's — an item that already started
    /// is no longer editable.
    pub fn edit(&mut self, message_id: &str, prompt: String) -> Result<(), RpcError> {
        let mut next = self.record.clone();
        let Some(item) = next
            .pending
            .iter_mut()
            .find(|item| item.message_id == message_id)
        else {
            return Err(self.mutation_refusal(message_id));
        };
        item.request.prompt = prompt;
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

    fn head(&self) -> Option<PendingMessage> {
        (!self.record.paused && !self.unreadable)
            .then(|| self.record.pending.first().cloned())
            .flatten()
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
        if next
            .pending
            .first()
            .is_none_or(|m| m.message_id != message_id)
        {
            return Err(RpcError::Failed("Message is no longer pending".into()));
        }
        let started = StartedMessage {
            message: next.pending.remove(0),
            timestamp,
        };
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

    fn finish(&mut self, success: bool, error: Option<String>) {
        let mut next = self.record.clone();
        let was_started = next.started.take().is_some();
        next.paused |= !success;
        if !was_started && let Some(head) = next.pending.first_mut() {
            head.error = error.clone();
        }
        if let Err(error) = self.commit(next) {
            self.record.paused = true;
            self.unreadable |= self.record.started.is_some();
            self.error = Some(error.to_string());
            self.publish();
        } else if was_started && let Some(error) = error {
            self.error = Some(error);
            self.publish();
        }
    }
}

impl EngineService {
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
                    )
                    .await;
                let (success, error) = match prepared {
                    Ok(run) => (run_agent_command(run).await, None),
                    Err(error) => (false, (!cancel.is_cancelled()).then(|| error.to_string())),
                };
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
                if !vanished && !worker_chat.is_removed() && !queue.unreadable {
                    // Stop already changed the pause state. A subsequent
                    // Continue must survive the canceled Turn's cleanup.
                    queue.finish(
                        (success || cancel.is_cancelled()) && persistence_error.is_none(),
                        persistence_error.or(error),
                    );
                }
                if started {
                    service.runtime.set_session(
                        &worker_chat.chat_id,
                        if success || cancel.is_cancelled() {
                            holt_proto::SessionStatus::Idle
                        } else {
                            holt_proto::SessionStatus::Errored
                        },
                    );
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

    fn queue_with_pending() -> (Queue, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut queue = Queue::load(dir.path(), "chat-1");
        queue
            .enqueue(request("B", "openai/gpt-5.4"), "m-b".into())
            .expect("enqueue B");
        queue
            .enqueue(request("C", "openai/gpt-5.4-mini"), "m-c".into())
            .expect("enqueue C");
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
        queue
            .enqueue(request("D", "openai/gpt-5.4"), "m-d".into())
            .expect("enqueue D");
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
}
