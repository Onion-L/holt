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

    pub fn start(&mut self, message_id: &str, timestamp: i64) -> Result<(), RpcError> {
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
        next.started = Some(StartedMessage {
            message: next.pending.remove(0),
            timestamp,
        });
        let result = self.commit(next);
        if result.is_err() && self.record.started.is_some() {
            // The rename landed but its directory sync did not. Recovery
            // must settle this checkpoint before another item can start.
            self.unreadable = true;
            self.record.paused = true;
            self.error = Some("Turn admission could not be confirmed. Restore storage and reopen Holt to recover the checkpoint.".into());
            self.publish();
        }
        result
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
                let (message, cancel) = {
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
                    (message, cancel)
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
                if !worker_chat.is_removed() && !queue.unreadable {
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
