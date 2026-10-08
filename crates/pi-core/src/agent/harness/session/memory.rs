//! Port of `pi-core/agent/src/harness/session/memory.rs` (the in-memory
//! storage and repo) plus `session.ts` (`Session`).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::agent::types::AgentMessage;
use crate::ai::utils::uuid::uuidv7;

use super::state::{SessionMutation, SessionState};
use super::types::{
    BranchBounds, Entry, EntryQuery, ForkOptions, LanePointer, LaneRecord, LogItem, LogOptions,
    RecordQuery, SessionCreateOptions, SessionError, SessionErrorCode, SessionMetadata,
    SessionStats, SessionStorage,
};

/// Injectable clock mirroring the TypeScript suites' `Date.now` overrides.
pub type ClockFn = std::sync::Arc<dyn Fn() -> i64 + Send + Sync>;

static CLOCK_OVERRIDE: std::sync::RwLock<Option<ClockFn>> = std::sync::RwLock::new(None);

/// Overrides the session clock (test seam; pass `None` to restore).
pub fn set_clock_override(clock: Option<ClockFn>) {
    *CLOCK_OVERRIDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = clock;
}

pub(crate) fn now_millis() -> i64 {
    if let Some(clock) = CLOCK_OVERRIDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        return clock();
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Port of `InMemorySessionStorage`.
pub struct InMemorySessionStorage {
    metadata: SessionMetadata,
    state: Mutex<SessionState>,
}

impl InMemorySessionStorage {
    pub fn new(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            state: Mutex::new(SessionState::new()),
        }
    }

    fn fork_storage(
        &self,
        metadata: SessionMetadata,
        options: &ForkOptions,
    ) -> Result<InMemorySessionStorage, SessionError> {
        let storage = InMemorySessionStorage::new(metadata);
        let mutations = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create_fork_mutations(options)?;
        {
            let mut fork_state = storage
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for mutation in mutations {
                fork_state.apply_mutation(mutation)?;
            }
        }
        Ok(storage)
    }
}

impl SessionStorage for InMemorySessionStorage {
    fn get_metadata(&self) -> BoxFuture<'static, SessionMetadata> {
        let metadata = self.metadata.clone();
        Box::pin(async move { metadata })
    }

    fn get_lanes(&self) -> BoxFuture<'static, Vec<LanePointer>> {
        let lanes = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_lanes();
        Box::pin(async move { lanes })
    }

    fn create_lane(
        &self,
        lane: String,
        at: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let result = {
            let mut state = lock(&self.state);
            let result = state
                .validate_new_lane(&lane)
                .and_then(|()| state.validate_target(at.as_deref()));
            match result {
                Err(error) => Err(error),
                Ok(()) => {
                    let seq = state.next_sequence();
                    state.apply_mutation(SessionMutation::Lane {
                        seq,
                        lane,
                        leaf_id: at,
                    })
                }
            }
        };
        Box::pin(async move { result })
    }

    fn move_lane(
        &self,
        lane: String,
        to: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let result = (|| {
            let mut state = lock(&self.state);
            state.require_lane(&lane)?;
            state.validate_target(to.as_deref())?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::Lane {
                seq,
                lane,
                leaf_id: to,
            })
        })();
        Box::pin(async move { result })
    }

    fn append_entry(
        &self,
        entry: Entry,
        lane: String,
    ) -> BoxFuture<'static, Result<Entry, SessionError>> {
        let result = (|| {
            let mut state = lock(&self.state);
            let parent_id = state.require_lane(&lane)?;
            state.validate_unused_id(entry.id())?;
            let seq = state.next_sequence();
            let entry = entry.with_storage_fields(parent_id, seq, now_millis());
            state.apply_mutation(SessionMutation::Entry {
                lane: Some(lane),
                entry: entry.clone(),
            })?;
            Ok(entry)
        })();
        Box::pin(async move { result })
    }

    fn append_record(
        &self,
        record: LaneRecord,
    ) -> BoxFuture<'static, Result<LaneRecord, SessionError>> {
        let result = (|| {
            let mut state = lock(&self.state);
            state.require_lane(record.lane())?;
            state.validate_unused_id(record.id())?;
            let current_open_operation = state
                .find_open_operations(record.lane(), Some(1))
                .unwrap_or_default()
                .into_iter()
                .next()
                .map(|record| record.id().to_string());
            if record.record_type() == super::types::RecordType::OperationStarted
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
            let seq = state.next_sequence();
            let timestamp = now_millis();
            let record = assign_record_storage(record, seq, timestamp);
            state.apply_mutation(SessionMutation::Record {
                record: record.clone(),
            })?;
            Ok(record)
        })();
        Box::pin(async move { result })
    }

    fn get_entry(&self, id: String) -> BoxFuture<'static, Option<Entry>> {
        let entry = lock(&self.state).get_entry(&id);
        Box::pin(async move { entry })
    }

    fn find_entries(&self, query: EntryQuery) -> BoxFuture<'static, Vec<Entry>> {
        let entries = lock(&self.state).find_entries(&query).unwrap_or_default();
        Box::pin(async move { entries })
    }

    fn find_entries_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
        start: String,
    ) -> BoxFuture<'static, Vec<Entry>> {
        let entries = lock(&self.state)
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
        let records = lock(&self.state).find_records(&query).unwrap_or_default();
        Box::pin(async move { records })
    }

    fn find_open_operations(
        &self,
        lane: String,
        limit: Option<usize>,
    ) -> BoxFuture<'static, Vec<LaneRecord>> {
        let records = lock(&self.state)
            .find_open_operations(&lane, limit)
            .unwrap_or_default();
        Box::pin(async move { records })
    }

    fn get_log(&self, options: LogOptions) -> BoxFuture<'static, Vec<LogItem>> {
        let log = lock(&self.state).get_log(&options).unwrap_or_default();
        Box::pin(async move { log })
    }

    fn get_name(&self) -> BoxFuture<'static, Option<String>> {
        let name = lock(&self.state).get_name();
        Box::pin(async move { name })
    }

    fn set_name(&self, name: Option<String>) -> BoxFuture<'static, Result<(), SessionError>> {
        let result = {
            let mut state = lock(&self.state);
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::NameFact { seq, name })
        };
        Box::pin(async move { result })
    }

    fn get_label(&self, id: String) -> BoxFuture<'static, Option<String>> {
        let label = lock(&self.state).get_label(&id);
        Box::pin(async move { label })
    }

    fn set_label(
        &self,
        id: String,
        label: Option<String>,
    ) -> BoxFuture<'static, Result<(), SessionError>> {
        let result = (|| {
            let mut state = lock(&self.state);
            state.validate_target(Some(&id))?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::LabelFact {
                seq,
                target_id: id,
                label,
            })
        })();
        Box::pin(async move { result })
    }

    fn get_stats(&self) -> BoxFuture<'static, SessionStats> {
        let stats = lock(&self.state).get_stats();
        Box::pin(async move { stats })
    }
}

fn assign_record_storage(mut record: LaneRecord, seq: u64, timestamp: i64) -> LaneRecord {
    fn assign(record: &mut LaneRecord, seq: u64, timestamp: i64) {
        match record {
            LaneRecord::OperationStarted {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::AbortRequested {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::OperationFinished {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::StepAttempt {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::ToolStarted {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::QueueEnqueued {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::QueueCancelled {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::WriteDeferred {
                seq: s,
                timestamp: t,
                ..
            }
            | LaneRecord::Usage {
                seq: s,
                timestamp: t,
                ..
            } => {
                *s = seq;
                *t = timestamp;
            }
        }
    }
    assign(&mut record, seq, timestamp);
    record
}

/// Port of `IdGenerator`.
pub type IdGenerator = Arc<dyn Fn() -> String + Send + Sync>;

fn uuid_id_generator() -> IdGenerator {
    Arc::new(uuidv7)
}

fn lock(state: &Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Port of the `assertValidLimit` half of the TypeScript session-level query
/// validation. `usize` limits are always integers, so only zero is invalid;
/// the TypeScript check additionally rejects fractional and negative values
/// the Rust query types cannot express.
fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
    if limit == Some(0) {
        return Err(SessionError::new(
            SessionErrorCode::InvalidQuery,
            "limit must be a positive integer",
        ));
    }
    Ok(())
}

/// Port of `Session`: the tree view over a storage.
#[derive(Clone)]
pub struct Session {
    storage: Arc<dyn SessionStorage>,
    id_generator: IdGenerator,
}

impl Session {
    pub fn new(storage: Arc<dyn SessionStorage>) -> Self {
        Self {
            storage,
            id_generator: uuid_id_generator(),
        }
    }

    pub fn with_id_generator(storage: Arc<dyn SessionStorage>, id_generator: IdGenerator) -> Self {
        Self {
            storage,
            id_generator,
        }
    }

    pub fn storage(&self) -> &Arc<dyn SessionStorage> {
        &self.storage
    }

    /// The injectable id generator (public readonly field in TypeScript).
    pub fn id_generator(&self) -> &IdGenerator {
        &self.id_generator
    }

    /// Port of `view`: a lane-scoped tree. `view("main")` mirrors the session
    /// itself, exactly like the TypeScript early return.
    pub fn view(&self, lane: &str) -> SessionView {
        SessionView {
            session: self.clone(),
            lane: lane.to_string(),
        }
    }

    pub async fn get_metadata(&self) -> SessionMetadata {
        self.storage.get_metadata().await
    }

    pub async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.get_leaf_id_for_lane("main").await
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.storage.get_entry(id.to_string()).await
    }

    pub async fn get_stats(&self) -> SessionStats {
        self.storage.get_stats().await
    }

    pub async fn get_name(&self) -> Option<String> {
        self.storage.get_name().await
    }

    pub async fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        self.storage.set_name(name).await
    }

    pub async fn get_label(&self, target_id: &str) -> Option<String> {
        self.storage.get_label(target_id.to_string()).await
    }

    pub async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<(), SessionError> {
        self.storage.set_label(target_id.to_string(), label).await
    }

    pub async fn find_entries(&self, query: EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.query_entries(query, None).await
    }

    pub async fn find_entry(&self, query: EntryQuery) -> Result<Option<Entry>, SessionError> {
        Ok(self.query_entries(query, Some(1)).await?.into_iter().next())
    }

    pub async fn find_entries_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.query_branch_entries("main", query, bounds, None).await
    }

    pub async fn find_entry_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        Ok(self
            .query_branch_entries("main", query, bounds, Some(1))
            .await?
            .into_iter()
            .next())
    }

    pub async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        self.append_message_to_lane("main", message).await
    }

    pub async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.append_custom_entry_to_lane("main", custom_type, data)
            .await
    }

    pub async fn get_lanes(&self) -> Vec<LanePointer> {
        self.storage.get_lanes().await
    }

    pub async fn create_lane(&self, lane: &str, at: Option<String>) -> Result<(), SessionError> {
        self.storage.create_lane(lane.to_string(), at).await
    }

    pub async fn move_lane(&self, lane: &str, to: Option<String>) -> Result<(), SessionError> {
        self.storage.move_lane(lane.to_string(), to).await
    }

    pub async fn append_entry(&self, entry: Entry, lane: &str) -> Result<Entry, SessionError> {
        self.commit_entry(entry, lane).await
    }

    pub async fn append_record(&self, record: LaneRecord) -> Result<LaneRecord, SessionError> {
        self.commit_record(record).await
    }

    pub async fn find_records(&self, query: RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(query.limit)?;
        if query.operation_kind.is_some()
            && query.record_type != Some(super::types::RecordType::OperationStarted)
        {
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "operationKind requires type \"operation_started\"",
            ));
        }
        Ok(self.storage.find_records(query).await)
    }

    pub async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(limit)?;
        Ok(self
            .storage
            .find_open_operations(lane.to_string(), limit)
            .await)
    }

    pub async fn get_log(&self, options: LogOptions) -> Result<Vec<LogItem>, SessionError> {
        assert_valid_limit(options.limit)?;
        Ok(self.storage.get_log(options).await)
    }

    async fn get_leaf_id_for_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        let lanes = self.storage.get_lanes().await;
        lanes
            .iter()
            .find(|pointer| pointer.lane == lane)
            .map(|pointer| pointer.leaf_id.clone())
            .ok_or_else(|| {
                SessionError::new(
                    SessionErrorCode::InvalidLane,
                    format!("Lane not found: {lane}"),
                )
            })
    }

    async fn query_entries(
        &self,
        query: EntryQuery,
        result_limit: Option<usize>,
    ) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        let mut query = query;
        if let Some(result_limit) = result_limit {
            query.limit = Some(result_limit);
        }
        Ok(self.storage.find_entries(query).await)
    }

    async fn query_branch_entries(
        &self,
        default_lane: &str,
        query: EntryQuery,
        bounds: BranchBounds,
        result_limit: Option<usize>,
    ) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        let mut bounds = bounds;
        if bounds.start.is_none() {
            bounds.start = self.get_leaf_id_for_lane(default_lane).await?;
        }
        let Some(start) = bounds.start.clone() else {
            return Ok(Vec::new());
        };
        // The TypeScript storage throws `not_found` from its branch walk;
        // the Rust storage trait is infallible on reads, so the session
        // performs the same missing-start check before delegating.
        if self.storage.get_entry(start.clone()).await.is_none() {
            return Err(SessionError::new(
                SessionErrorCode::NotFound,
                format!("Entry not found: {start}"),
            ));
        }
        let mut query = query;
        if let Some(result_limit) = result_limit {
            query.limit = Some(result_limit);
        }
        Ok(self
            .storage
            .find_entries_on_branch(query, bounds, start)
            .await)
    }

    async fn append_message_to_lane(
        &self,
        lane: &str,
        message: AgentMessage,
    ) -> Result<String, SessionError> {
        let entry = self
            .commit_entry(
                Entry::Message {
                    id: (self.id_generator)(),
                    seq: 0,
                    parent_id: None,
                    timestamp: 0,
                    message,
                    terminate: None,
                },
                lane,
            )
            .await?;
        Ok(entry.id().to_string())
    }

    async fn append_custom_entry_to_lane(
        &self,
        lane: &str,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let entry = self
            .commit_entry(
                Entry::Custom {
                    id: (self.id_generator)(),
                    seq: 0,
                    parent_id: None,
                    timestamp: 0,
                    custom_type: custom_type.to_string(),
                    data,
                },
                lane,
            )
            .await?;
        Ok(entry.id().to_string())
    }

    async fn commit_entry(&self, entry: Entry, lane: &str) -> Result<Entry, SessionError> {
        // The JSON-serializability of the Rust entry types is enforced by
        // construction (`assertJsonSerializable` guards the dynamic
        // TypeScript payloads).
        self.storage.append_entry(entry, lane.to_string()).await
    }

    async fn commit_record(&self, record: LaneRecord) -> Result<LaneRecord, SessionError> {
        self.storage.append_record(record).await
    }
}

/// Port of the `SessionTree` lane view returned by `Session::view`. Reads
/// and appends are lane-scoped; the remaining methods delegate to the whole
/// session exactly like the TypeScript view object.
pub struct SessionView {
    session: Session,
    lane: String,
}

impl SessionView {
    pub async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.session.get_leaf_id_for_lane(&self.lane).await
    }

    pub async fn get_entry(&self, id: &str) -> Option<Entry> {
        self.session.get_entry(id).await
    }

    pub async fn get_stats(&self) -> SessionStats {
        self.session.get_stats().await
    }

    pub async fn get_name(&self) -> Option<String> {
        self.session.get_name().await
    }

    pub async fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        self.session.set_name(name).await
    }

    pub async fn get_label(&self, target_id: &str) -> Option<String> {
        self.session.get_label(target_id).await
    }

    pub async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<(), SessionError> {
        self.session.set_label(target_id, label).await
    }

    pub async fn find_entries(&self, query: EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.session.query_entries(query, None).await
    }

    pub async fn find_entry(&self, query: EntryQuery) -> Result<Option<Entry>, SessionError> {
        self.session
            .query_entries(query, Some(1))
            .await
            .map(|entries| entries.into_iter().next())
    }

    pub async fn find_entries_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.session
            .query_branch_entries(&self.lane, query, bounds, None)
            .await
    }

    pub async fn find_entry_on_branch(
        &self,
        query: EntryQuery,
        bounds: BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        self.session
            .query_branch_entries(&self.lane, query, bounds, Some(1))
            .await
            .map(|entries| entries.into_iter().next())
    }

    pub async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        self.session
            .append_message_to_lane(&self.lane, message)
            .await
    }

    pub async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.session
            .append_custom_entry_to_lane(&self.lane, custom_type, data)
            .await
    }
}

/// Port of `InMemorySessionRepo`.
#[derive(Default)]
pub struct InMemorySessionRepo {
    sessions: Mutex<std::collections::HashMap<String, Arc<InMemorySessionStorage>>>,
}

impl InMemorySessionRepo {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create(&self, options: SessionCreateOptions) -> Result<Session, SessionError> {
        let id = options.id.clone().unwrap_or_else(uuidv7);
        let storage = Arc::new(InMemorySessionStorage::new(SessionMetadata {
            id: id.clone(),
            created_at: now_millis(),
            parent_session_id: options.parent_session_id,
        }));
        let mut sessions = lock_repo(&self.sessions);
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }
        sessions.insert(id, Arc::clone(&storage));
        Ok(Session::new(storage))
    }

    pub async fn open(&self, metadata: SessionMetadata) -> Result<Session, SessionError> {
        Ok(Session::new(self.require_storage(&metadata.id)?))
    }

    pub async fn list(&self) -> Vec<SessionMetadata> {
        let mut metadata: Vec<SessionMetadata> = lock_repo(&self.sessions)
            .values()
            .map(|storage| futures::executor::block_on(storage.get_metadata()))
            .collect();
        metadata.sort_by_key(|metadata| metadata.created_at);
        metadata
    }

    pub async fn delete(&self, metadata: SessionMetadata) {
        lock_repo(&self.sessions).remove(&metadata.id);
    }

    pub async fn fork(
        &self,
        source: SessionMetadata,
        options: ForkOptions,
        create_options: SessionCreateOptions,
    ) -> Result<Session, SessionError> {
        let source_storage = self.require_storage(&source.id)?;
        let id = create_options.id.clone().unwrap_or_else(uuidv7);
        if lock_repo(&self.sessions).contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }
        let storage = Arc::new(
            source_storage.fork_storage(
                SessionMetadata {
                    id: id.clone(),
                    created_at: now_millis(),
                    parent_session_id: create_options
                        .parent_session_id
                        .or_else(|| Some(source.id.clone())),
                },
                &options,
            )?,
        );
        lock_repo(&self.sessions).insert(id, Arc::clone(&storage));
        Ok(Session::new(storage))
    }

    fn require_storage(&self, id: &str) -> Result<Arc<InMemorySessionStorage>, SessionError> {
        lock_repo(&self.sessions).get(id).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::NotFound,
                format!("Session not found: {id}"),
            )
        })
    }
}

fn lock_repo(
    sessions: &Mutex<std::collections::HashMap<String, Arc<InMemorySessionStorage>>>,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Arc<InMemorySessionStorage>>> {
    sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
