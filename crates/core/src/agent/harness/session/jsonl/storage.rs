//! Port of `pi-core/agent/src/harness/session/jsonl/storage.ts`.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::agent::harness::types::{FileSystem, RemoveOptions, WriteContent};

use super::super::memory::now_millis;
use super::super::state::{SessionMutation, SessionState};
use super::super::types::{
    BranchBounds, Entry, EntryQuery, ForkOptions, LanePointer, LaneRecord, LogItem, LogOptions,
    RecordQuery, RecordType, SessionError, SessionErrorCode, SessionMetadata, SessionStats,
    SessionStorage,
};
use super::codec::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
};
use super::errors::{JsonlDecodeError, JsonlDecodeErrorKind, file_err, file_result, invalid_file};
use super::types::{JsonlSessionMetadata, JsonlV4Header};

fn lock_state(state: &std::sync::Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn force_remove(fs: &Arc<dyn FileSystem>, path: &str) -> BoxFuture<'static, ()> {
    let fs = Arc::clone(fs);
    let path = path.to_string();
    Box::pin(async move {
        let _ = fs
            .remove(
                &path,
                RemoveOptions {
                    force: Some(true),
                    recursive: None,
                },
                None,
            )
            .await;
    })
}

/// Build a complete sibling temporary file, then atomically rename it over
/// the destination (port of `publishFileAtomically`). The original error is
/// preserved when population or rename fails; temporary-file removal is
/// best-effort.
async fn publish_file_atomically(
    fs: &Arc<dyn FileSystem>,
    destination_path: &str,
    populate: impl FnOnce(String) -> BoxFuture<'static, Result<(), SessionError>>,
) -> Result<(), SessionError> {
    let temp_path = format!("{destination_path}.tmp");
    match populate(temp_path.clone()).await {
        Ok(()) => fs
            .rename_file(&temp_path, destination_path, None)
            .await
            .map_err(|error| {
                file_err(
                    error,
                    &format!("Failed to publish staged file {destination_path}"),
                )
            }),
        Err(error) => {
            force_remove(fs, &temp_path).await;
            Err(error)
        }
    }
}

struct JsonlSessionInner {
    fs: Arc<dyn FileSystem>,
    metadata: JsonlSessionMetadata,
    state: std::sync::Mutex<SessionState>,
    /// Serializes mutations the way the TypeScript promise tail does.
    write_tail: tokio::sync::Mutex<()>,
}

/// Port of `JsonlSessionStorage`.
pub struct JsonlSessionStorage {
    inner: Arc<JsonlSessionInner>,
}

impl JsonlSessionStorage {
    pub fn new(fs: Arc<dyn FileSystem>, metadata: JsonlSessionMetadata) -> Self {
        Self {
            inner: Arc::new(JsonlSessionInner {
                fs,
                metadata,
                state: std::sync::Mutex::new(SessionState::new()),
                write_tail: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Port of `JsonlSessionStorage.create`.
    pub async fn create(
        fs: Arc<dyn FileSystem>,
        path: &str,
        header: JsonlV4Header,
    ) -> Result<Arc<Self>, SessionError> {
        fs.write_file(path, &WriteContent::Text(encode_header(&header)), None)
            .await
            .map_err(|error| file_err(error, &format!("Failed to initialize session {path}")))?;
        let file_info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        Ok(Arc::new(Self::new(
            fs,
            metadata_from_header(&header, path, file_info.mtime_ms),
        )))
    }

    /// Port of `JsonlSessionStorage.load`, including torn-tail and
    /// unterminated-tail repair.
    pub async fn load(fs: Arc<dyn FileSystem>, path: &str) -> Result<Arc<Self>, SessionError> {
        let content = file_result(
            fs.read_text_file(path, None).await,
            &format!("Failed to read session {path}"),
        )?;
        let mut physical_lines: Vec<&str> = content.split('\n').collect();
        if physical_lines.last() == Some(&"") {
            physical_lines.pop();
        }
        if physical_lines.is_empty() || physical_lines[0].is_empty() {
            return Err(invalid_file(
                path,
                1,
                &JsonlDecodeError::schema("is missing a header"),
            ));
        }
        let header =
            parse_header(physical_lines[0]).map_err(|error| invalid_file(path, 1, &error))?;
        let file_info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let storage = Self::new(
            Arc::clone(&fs),
            metadata_from_header(&header, path, file_info.mtime_ms),
        );
        for index in 1..physical_lines.len() {
            let line = physical_lines[index];
            let mutation = match parse_mutation(line) {
                Ok(mutation) => mutation,
                Err(error) => {
                    let is_torn_tail = index == physical_lines.len() - 1
                        && error.kind == JsonlDecodeErrorKind::Syntax;
                    if is_torn_tail {
                        // Drop the unacknowledged partial append by
                        // atomically publishing the valid prefix.
                        let valid_prefix = format!("{}\n", physical_lines[..index].join("\n"));
                        let repair_fs = Arc::clone(&fs);
                        let repair_path = path.to_string();
                        publish_file_atomically(&fs, path, move |temp_path| {
                            Box::pin(async move {
                                repair_fs
                                    .write_file(&temp_path, &WriteContent::Text(valid_prefix), None)
                                    .await
                                    .map_err(|error| {
                                        file_err(
                                            error,
                                            &format!(
                                                "Failed to stage torn-tail repair {repair_path}"
                                            ),
                                        )
                                    })
                            })
                        })
                        .await?;
                        return Ok(Arc::new(storage));
                    }
                    return Err(invalid_file(path, index + 1, &error));
                }
            };
            if let Err(error) = lock_state(&storage.inner.state).apply_mutation(mutation) {
                if error.code == SessionErrorCode::InvalidEntry {
                    return Err(invalid_file(
                        path,
                        index + 1,
                        &JsonlDecodeError::schema(error.message),
                    ));
                }
                return Err(error);
            }
        }
        if !content.ends_with('\n') {
            fs.append_file(path, &WriteContent::Text("\n".to_string()), None)
                .await
                .map_err(|error| {
                    file_err(
                        error,
                        &format!("Failed to repair unterminated session tail {path}"),
                    )
                })?;
        }
        Ok(Arc::new(storage))
    }

    /// Port of `fork`.
    pub async fn fork(
        self: &Arc<Self>,
        path: &str,
        header: JsonlV4Header,
        options: &ForkOptions,
    ) -> Result<Arc<Self>, SessionError> {
        let mutations = lock_state(&self.inner.state).create_fork_mutations(options)?;
        let fs = Arc::clone(&self.inner.fs);
        publish_file_atomically(&self.inner.fs, path, move |temp_path| {
            Box::pin(async move {
                let target_storage = Self::create(Arc::clone(&fs), &temp_path, header).await?;
                for mutation in mutations {
                    target_storage.append_mutation(&mutation).await?;
                    lock_state(&target_storage.inner.state).apply_mutation(mutation)?;
                }
                Ok(())
            })
        })
        .await?;
        Self::load(Arc::clone(&self.inner.fs), path).await
    }

    /// Port of `drain`.
    pub async fn drain(&self) {
        let _guard = self.inner.write_tail.lock().await;
    }

    /// The JSONL-specific session metadata.
    pub fn jsonl_metadata(&self) -> JsonlSessionMetadata {
        self.inner.metadata.clone()
    }

    async fn append_mutation(&self, mutation: &SessionMutation) -> Result<(), SessionError> {
        self.inner
            .fs
            .append_file(
                &self.inner.metadata.path,
                &WriteContent::Text(encode_mutation(mutation)),
                None,
            )
            .await
            .map_err(|error| {
                file_err(
                    error,
                    &format!("Failed to append session {}", self.inner.metadata.path),
                )
            })
    }
}

impl SessionStorage for JsonlSessionStorage {
    fn get_metadata(&self) -> BoxFuture<'static, SessionMetadata> {
        // The trait's metadata type is the base `SessionMetadata`; the
        // JSONL-specific fields ride on `jsonl_metadata`.
        let metadata = SessionMetadata {
            id: self.inner.metadata.id.clone(),
            created_at: self.inner.metadata.created_at,
            parent_session_id: self.inner.metadata.parent_session_id.clone(),
        };
        Box::pin(async move { metadata })
    }

    fn get_lanes(&self) -> BoxFuture<'static, Vec<LanePointer>> {
        let lanes = lock_state(&self.inner.state).get_lanes();
        Box::pin(async move { lanes })
    }

    fn create_lane(
        &self,
        lane: String,
        at: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            {
                let state = lock_state(&storage.inner.state);
                state.validate_new_lane(&lane)?;
                state.validate_target(at.as_deref())?;
            }
            let seq = lock_state(&storage.inner.state).next_sequence();
            let mutation = SessionMutation::Lane {
                seq,
                lane,
                leaf_id: at,
            };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)
        })
    }

    fn move_lane(
        &self,
        lane: String,
        to: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            {
                let state = lock_state(&storage.inner.state);
                state.require_lane(&lane)?;
                state.validate_target(to.as_deref())?;
            }
            let seq = lock_state(&storage.inner.state).next_sequence();
            let mutation = SessionMutation::Lane {
                seq,
                lane,
                leaf_id: to,
            };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)
        })
    }

    fn append_entry(
        &self,
        entry: Entry,
        lane: String,
    ) -> BoxFuture<'static, Result<Entry, SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            let (parent_id, seq) = {
                let state = lock_state(&storage.inner.state);
                let parent_id = state.require_lane(&lane)?;
                state.validate_unused_id(entry.id())?;
                (parent_id, state.next_sequence())
            };
            let entry = entry.with_storage_fields(parent_id, seq, now_millis());
            let mutation = SessionMutation::Entry {
                lane: Some(lane),
                entry: entry.clone(),
            };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)?;
            Ok(entry)
        })
    }

    fn append_record(
        &self,
        record: LaneRecord,
    ) -> BoxFuture<'static, Result<LaneRecord, SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            let seq = {
                let state = lock_state(&storage.inner.state);
                state.require_lane(record.lane())?;
                state.validate_unused_id(record.id())?;
                let current_open_operation = state
                    .find_open_operations(record.lane(), Some(1))
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                    .map(|record| record.id().to_string());
                if record.record_type() == RecordType::OperationStarted
                    && let Some(current) = current_open_operation
                {
                    return Err(SessionError::new(
                        SessionErrorCode::Storage,
                        format!(
                            "Lane {} already has an open operation {}",
                            record.lane(),
                            current
                        ),
                    ));
                }
                state.next_sequence()
            };
            let record = assign_record_storage(record, seq, now_millis());
            let mutation = SessionMutation::Record {
                record: record.clone(),
            };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)?;
            Ok(record)
        })
    }

    fn get_entry(&self, id: String) -> BoxFuture<'static, Option<Entry>> {
        let entry = lock_state(&self.inner.state).get_entry(&id);
        Box::pin(async move { entry })
    }

    fn find_entries(&self, query: EntryQuery) -> BoxFuture<'static, Vec<Entry>> {
        let entries = lock_state(&self.inner.state)
            .find_entries(&query)
            .unwrap_or_default();
        Box::pin(async move { entries })
    }

    fn find_entries_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
        start: String,
    ) -> BoxFuture<'static, Vec<Entry>> {
        let entries = lock_state(&self.inner.state)
            .find_entries_on_branch(
                &query,
                bounds.stop_at_id.as_deref(),
                bounds.stop_at_type,
                &start,
            )
            .unwrap_or_default();
        Box::pin(async move { entries })
    }

    fn find_records(&self, query: RecordQuery) -> BoxFuture<'static, Vec<LaneRecord>> {
        let records = lock_state(&self.inner.state)
            .find_records(&query)
            .unwrap_or_default();
        Box::pin(async move { records })
    }

    fn find_open_operations(
        &self,
        lane: String,
        limit: Option<usize>,
    ) -> BoxFuture<'static, Vec<LaneRecord>> {
        let records = lock_state(&self.inner.state)
            .find_open_operations(&lane, limit)
            .unwrap_or_default();
        Box::pin(async move { records })
    }

    fn get_log(&self, options: LogOptions) -> BoxFuture<'static, Vec<LogItem>> {
        let log = lock_state(&self.inner.state)
            .get_log(&options)
            .unwrap_or_default();
        Box::pin(async move { log })
    }

    fn get_name(&self) -> BoxFuture<'static, Option<String>> {
        let name = lock_state(&self.inner.state).get_name();
        Box::pin(async move { name })
    }

    fn set_name(&self, name: Option<String>) -> BoxFuture<'static, Result<(), SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            let seq = lock_state(&storage.inner.state).next_sequence();
            let mutation = SessionMutation::NameFact { seq, name };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)
        })
    }

    fn get_label(&self, id: String) -> BoxFuture<'static, Option<String>> {
        let label = lock_state(&self.inner.state).get_label(&id);
        Box::pin(async move { label })
    }

    fn set_label(
        &self,
        id: String,
        label: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let storage = JsonlSessionStorage {
                inner: Arc::clone(&inner),
            };
            let _guard = inner.write_tail.lock().await;
            let seq = {
                let state = lock_state(&storage.inner.state);
                state.validate_target(Some(&id))?;
                state.next_sequence()
            };
            let mutation = SessionMutation::LabelFact {
                seq,
                target_id: id,
                label,
            };
            storage.append_mutation(&mutation).await?;
            lock_state(&storage.inner.state).apply_mutation(mutation)
        })
    }

    fn get_stats(&self) -> BoxFuture<'static, SessionStats> {
        let stats = lock_state(&self.inner.state).get_stats();
        Box::pin(async move { stats })
    }
}

fn assign_record_storage(mut record: LaneRecord, seq: u64, timestamp: i64) -> LaneRecord {
    match &mut record {
        LaneRecord::OperationStarted {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::AbortRequested {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::OperationFinished {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::StepAttempt {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::ToolStarted {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::QueueEnqueued {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::QueueCancelled {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::WriteDeferred {
            seq: target,
            timestamp: time,
            ..
        }
        | LaneRecord::Usage {
            seq: target,
            timestamp: time,
            ..
        } => {
            *target = seq;
            *time = timestamp;
        }
    }
    record
}
