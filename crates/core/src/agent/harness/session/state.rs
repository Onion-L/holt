//! Port of `pi-core/agent/src/harness/session/state.ts`.

use std::collections::{HashMap, HashSet};

use super::types::{
    Entry, EntryOrder, EntryQuery, EntryType, ForkOptions, ForkPosition, LanePointer, LaneRecord,
    LogItem, LogOptions, RecordQuery, RecordType, SessionError, SessionErrorCode, SessionStats,
};

/// Port of `SessionMutation`.
#[derive(Clone, Debug)]
pub enum SessionMutation {
    Entry {
        lane: Option<String>,
        entry: Entry,
    },
    Record {
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    NameFact {
        seq: u64,
        name: Option<String>,
    },
    LabelFact {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

fn invalid_mutation(message: impl std::fmt::Display) -> SessionError {
    SessionError::new(
        SessionErrorCode::InvalidEntry,
        format!("Invalid session mutation: {message}"),
    )
}

fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
    if let Some(limit) = limit
        && limit == 0
    {
        return Err(SessionError::new(
            SessionErrorCode::InvalidQuery,
            "limit must be a positive integer",
        ));
    }
    Ok(())
}

fn assert_valid_cursor(_after_seq: Option<u64>) -> Result<(), SessionError> {
    // u64 cursors are always non-negative integers; the TypeScript check
    // guards against fractional or negative numbers.
    Ok(())
}

fn ordered<T: Clone>(items: &[T], order: Option<EntryOrder>) -> Vec<T> {
    // An `undefined` order defaults to newest-first, matching the
    // TypeScript `EntryQuery.order` / `RecordQuery.order` default.
    match order {
        Some(EntryOrder::OldestFirst) => items.to_vec(),
        Some(EntryOrder::NewestFirst) | None => items.iter().rev().cloned().collect(),
    }
}

/// Port of `SessionState`: the in-memory session tree with validation.
#[derive(Default)]
pub struct SessionState {
    sequence: u64,
    used_ids: HashSet<String>,
    entries: Vec<Entry>,
    entries_by_id: HashMap<String, Entry>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: HashMap<String, Vec<LaneRecord>>,
    lanes: Vec<LanePointer>,
    log: Vec<LogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            lanes: vec![LanePointer {
                lane: "main".to_string(),
                leaf_id: None,
            }],
            ..Default::default()
        }
    }

    /// The sequence the next mutation will take.
    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes.clone()
    }

    pub fn require_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        self.lanes
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

    pub fn validate_new_lane(&self, lane: &str) -> Result<(), SessionError> {
        if self.lanes.iter().any(|pointer| pointer.lane == lane) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Lane already exists: {lane}"),
            ));
        }
        Ok(())
    }

    pub fn validate_target(&self, target_id: Option<&str>) -> Result<(), SessionError> {
        if let Some(target_id) = target_id
            && !self.entries_by_id.contains_key(target_id)
        {
            return Err(SessionError::new(
                SessionErrorCode::NotFound,
                format!("Entry not found: {target_id}"),
            ));
        }
        Ok(())
    }

    pub fn validate_unused_id(&self, id: &str) -> Result<(), SessionError> {
        if self.used_ids.contains(id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session id already exists: {id}"),
            ));
        }
        Ok(())
    }

    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> Result<(), SessionError> {
        let seq = match &mutation {
            SessionMutation::Entry { entry, .. } => entry.seq(),
            SessionMutation::Record { record } => record.seq(),
            SessionMutation::Lane { seq, .. }
            | SessionMutation::NameFact { seq, .. }
            | SessionMutation::LabelFact { seq, .. } => *seq,
        };
        if seq != self.sequence + 1 {
            return Err(invalid_mutation(format!("has non-consecutive seq {seq}")));
        }

        match mutation {
            SessionMutation::Entry { lane, entry } => {
                if self.used_ids.contains(entry.id()) {
                    return Err(invalid_mutation(format!(
                        "contains duplicate id {}",
                        entry.id()
                    )));
                }
                if let Some(lane) = &lane {
                    let Some(pointer) = self.lanes.iter().find(|pointer| &pointer.lane == lane)
                    else {
                        return Err(invalid_mutation(format!("references missing lane {lane}")));
                    };
                    if entry.parent_id() != pointer.leaf_id.as_deref() {
                        return Err(invalid_mutation("does not chain to the lane leaf"));
                    }
                }
                if let Some(parent_id) = entry.parent_id()
                    && !self.entries_by_id.contains_key(parent_id)
                {
                    return Err(invalid_mutation(format!(
                        "references missing parent {parent_id}"
                    )));
                }
                self.sequence = seq;
                self.used_ids.insert(entry.id().to_string());
                let id = entry.id().to_string();
                if let Some(lane) = lane
                    && let Some(pointer) = self.lanes.iter_mut().find(|p| p.lane == lane)
                {
                    pointer.leaf_id = Some(id.clone());
                }
                if entry.entry_type() == EntryType::Message {
                    self.stats.message_count += 1;
                }
                self.log.push(LogItem::Entry {
                    seq,
                    entry: entry.clone(),
                });
                self.entries.push(entry.clone());
                self.entries_by_id.insert(id, entry);
            }
            SessionMutation::Record { record } => {
                if !self
                    .lanes
                    .iter()
                    .any(|pointer| pointer.lane == record.lane())
                {
                    return Err(invalid_mutation(format!(
                        "references missing lane {}",
                        record.lane()
                    )));
                }
                if self.used_ids.contains(record.id()) {
                    return Err(invalid_mutation(format!(
                        "contains duplicate id {}",
                        record.id()
                    )));
                }
                self.sequence = seq;
                self.used_ids.insert(record.id().to_string());
                match record.record_type() {
                    RecordType::OperationStarted => {
                        let open_operations = self
                            .open_operations_by_lane
                            .entry(record.lane().to_string())
                            .or_default();
                        open_operations.push(record.clone());
                    }
                    RecordType::OperationFinished => {
                        if let Some(open_operations) =
                            self.open_operations_by_lane.get_mut(record.lane())
                        {
                            open_operations
                                .retain(|open| open.id() != record.run_id().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
                if record.record_type() == RecordType::Usage
                    && let LaneRecord::Usage { usage, .. } = &record
                {
                    self.stats.cached_tokens += usage.cache_read;
                    self.stats.uncached_tokens += usage.input + usage.cache_write;
                    self.stats.total_tokens += usage.total_tokens;
                    self.stats.cost_total += usage.cost.total.0;
                }
                self.log.push(LogItem::Record {
                    seq,
                    record: record.clone(),
                });
                self.records.push(record);
            }
            SessionMutation::Lane { seq, lane, leaf_id } => {
                if let Some(leaf_id) = &leaf_id
                    && !self.entries_by_id.contains_key(leaf_id)
                {
                    return Err(invalid_mutation(format!(
                        "references missing lane target {leaf_id}"
                    )));
                }
                self.sequence = seq;
                let lane_pointer = LanePointer {
                    lane: lane.clone(),
                    leaf_id: leaf_id.clone(),
                };
                match self.lanes.iter_mut().find(|pointer| pointer.lane == lane) {
                    Some(pointer) => pointer.leaf_id = leaf_id.clone(),
                    None => self.lanes.push(lane_pointer),
                }
                self.log.push(LogItem::Lane { seq, lane, leaf_id });
            }
            SessionMutation::NameFact { seq, name } => {
                self.sequence = seq;
                self.name = name.clone();
                self.log.push(LogItem::NameFact { seq, name });
            }
            SessionMutation::LabelFact {
                seq,
                target_id,
                label,
            } => {
                if !self.entries_by_id.contains_key(&target_id) {
                    return Err(invalid_mutation(format!(
                        "references missing label target {target_id}"
                    )));
                }
                self.sequence = seq;
                match &label {
                    Some(label) => {
                        self.labels.insert(target_id.clone(), label.clone());
                    }
                    None => {
                        self.labels.remove(&target_id);
                    }
                }
                self.log.push(LogItem::LabelFact {
                    seq,
                    target_id,
                    label,
                });
            }
        }
        Ok(())
    }

    pub fn get_entry(&self, id: &str) -> Option<Entry> {
        self.entries_by_id.get(id).cloned()
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.cursor.map(|cursor| cursor.after_seq))?;
        let mut results: Vec<Entry> = Vec::new();
        for entry in ordered(&self.entries, query.order) {
            if !self.matches_entry_query(&entry, query) {
                continue;
            }
            results.push(entry);
            if Some(results.len()) == query.limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        stop_at_id: Option<&str>,
        stop_at_type: Option<EntryType>,
        start: &str,
    ) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.cursor.map(|cursor| cursor.after_seq))?;
        let mut results: Vec<Entry> = Vec::new();
        if query.order == Some(EntryOrder::OldestFirst) {
            // The TypeScript oldest-first branch walks to the root without
            // bounds and applies the inclusive stop bound while iterating,
            // keeping the root-side of the bound; reproduce that exactly.
            let mut path = self.walk_to_root(Some(start), None, None)?;
            path.reverse();
            for entry in path {
                let reached_bound =
                    Some(entry.id()) == stop_at_id || Some(entry.entry_type()) == stop_at_type;
                if self.matches_entry_query(&entry, query) {
                    results.push(entry);
                }
                if reached_bound || Some(results.len()) == query.limit {
                    break;
                }
            }
        } else {
            for entry in self.walk_to_root(Some(start), stop_at_id, stop_at_type)? {
                if self.matches_entry_query(&entry, query) {
                    results.push(entry);
                }
                if Some(results.len()) == query.limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        let mut results: Vec<LaneRecord> = Vec::new();
        for record in ordered(&self.records, query.order) {
            if !self.matches_record_query(&record, query) {
                continue;
            }
            results.push(record);
            if Some(results.len()) == query.limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(limit)?;
        let mut open_operations: Vec<LaneRecord> = self
            .open_operations_by_lane
            .get(lane)
            .cloned()
            .unwrap_or_default();
        open_operations.reverse();
        if let Some(limit) = limit {
            open_operations.truncate(limit);
        }
        Ok(open_operations)
    }

    pub fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        assert_valid_limit(options.limit)?;
        assert_valid_cursor(options.after_seq)?;
        let mut results: Vec<LogItem> = Vec::new();
        for item in &self.log {
            if let Some(after_seq) = options.after_seq
                && item_seq(item) <= after_seq
            {
                continue;
            }
            results.push(item.clone());
            if Some(results.len()) == options.limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn get_name(&self) -> Option<String> {
        self.name.clone()
    }

    pub fn get_label(&self, id: &str) -> Option<String> {
        self.labels.get(id).cloned()
    }

    pub fn get_stats(&self) -> SessionStats {
        self.stats
    }

    /// Port of `createForkMutations`.
    pub fn create_fork_mutations(
        &self,
        options: &ForkOptions,
    ) -> Result<Vec<SessionMutation>, SessionError> {
        let (copied_entries, fork_lanes): (Vec<Entry>, Vec<LanePointer>) = match options {
            ForkOptions::Tree => (
                self.find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })?,
                self.get_lanes(),
            ),
            ForkOptions::Branch { entry_id, position } => {
                let selected_entry_id = match entry_id {
                    Some(entry_id) => Some(entry_id.clone()),
                    None => self.require_lane("main")?,
                };
                let mut target_id: Option<String> = None;
                if let Some(selected_entry_id) = selected_entry_id {
                    let entry = self.get_entry(&selected_entry_id).ok_or_else(|| {
                        SessionError::new(
                            SessionErrorCode::InvalidForkTarget,
                            format!("Fork target is not a message entry: {selected_entry_id}"),
                        )
                    })?;
                    if entry.entry_type() != EntryType::Message {
                        return Err(SessionError::new(
                            SessionErrorCode::InvalidForkTarget,
                            format!("Fork target is not a message entry: {selected_entry_id}"),
                        ));
                    }
                    let position = position.unwrap_or(match entry_id {
                        None => ForkPosition::At,
                        Some(_) => ForkPosition::Before,
                    });
                    target_id = match position {
                        ForkPosition::At => Some(entry.id().to_string()),
                        ForkPosition::Before => entry.parent_id().map(str::to_string),
                    };
                }
                let copied_entries = match &target_id {
                    None => Vec::new(),
                    Some(target_id) => self.find_entries_on_branch(
                        &EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        None,
                        None,
                        target_id,
                    )?,
                };
                (
                    copied_entries,
                    vec![LanePointer {
                        lane: "main".to_string(),
                        leaf_id: target_id,
                    }],
                )
            }
        };

        let mut mutations: Vec<SessionMutation> = Vec::new();
        let mut sequence = 1u64;
        for source_entry in &copied_entries {
            let mut cloned = source_entry.clone();
            assign_seq(&mut cloned, sequence);
            sequence += 1;
            mutations.push(SessionMutation::Entry {
                lane: None,
                entry: cloned,
            });
        }
        for pointer in fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane: pointer.lane,
                leaf_id: pointer.leaf_id,
            });
            sequence += 1;
        }
        if let Some(name) = &self.name {
            mutations.push(SessionMutation::NameFact {
                seq: sequence,
                name: Some(name.clone()),
            });
            sequence += 1;
        }
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(entry.id()) {
                mutations.push(SessionMutation::LabelFact {
                    seq: sequence,
                    target_id: entry.id().to_string(),
                    label: Some(label.clone()),
                });
                sequence += 1;
            }
        }
        Ok(mutations)
    }

    fn walk_to_root(
        &self,
        start: Option<&str>,
        stop_at_id: Option<&str>,
        stop_at_type: Option<EntryType>,
    ) -> Result<Vec<Entry>, SessionError> {
        let Some(start) = start else {
            return Ok(Vec::new());
        };
        let mut visited: HashSet<String> = HashSet::new();
        let mut path: Vec<Entry> = Vec::new();
        let mut current = self.entries_by_id.get(start).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::NotFound,
                format!("Entry not found: {start}"),
            )
        })?;
        loop {
            if visited.contains(current.id()) {
                return Err(SessionError::new(
                    SessionErrorCode::InvalidEntry,
                    format!("Session branch contains a cycle at {}", current.id()),
                ));
            }
            visited.insert(current.id().to_string());
            let parent_id = current.parent_id().map(str::to_string);
            let reached_bound =
                Some(current.id()) == stop_at_id || Some(current.entry_type()) == stop_at_type;
            path.push(current.clone());
            if reached_bound || parent_id.is_none() {
                break;
            }
            let parent_id = parent_id.unwrap_or_default();
            current = self.entries_by_id.get(&parent_id).cloned().ok_or_else(|| {
                SessionError::new(
                    SessionErrorCode::InvalidEntry,
                    format!("Entry not found: {parent_id}"),
                )
            })?;
        }
        Ok(path)
    }

    fn matches_entry_query(&self, entry: &Entry, query: &EntryQuery) -> bool {
        (query.entry_type.is_none() || Some(entry.entry_type()) == query.entry_type)
            && (query.custom_type.is_none() || {
                match entry {
                    Entry::Custom { custom_type, .. } => {
                        custom_type == query.custom_type.as_deref().unwrap_or("")
                    }
                    _ => false,
                }
            })
            && (query.cursor.is_none() || {
                let after_seq = query
                    .cursor
                    .map(|cursor| cursor.after_seq)
                    .unwrap_or_default();
                match query.order {
                    Some(EntryOrder::OldestFirst) => entry.seq() > after_seq,
                    _ => entry.seq() < after_seq,
                }
            })
    }

    fn matches_record_query(&self, record: &LaneRecord, query: &RecordQuery) -> bool {
        (query.lane.is_none() || Some(record.lane()) == query.lane.as_deref())
            && (query.record_type.is_none() || Some(record.record_type()) == query.record_type)
            && (query.run_id.is_none() || {
                let run_id = query.run_id.as_deref().unwrap_or("");
                if record.record_type() == RecordType::OperationStarted {
                    record.id() == run_id
                } else {
                    record.run_id() == Some(run_id)
                }
            })
            && (query.operation_kind.is_none() || {
                match record {
                    LaneRecord::OperationStarted { intent, .. } => {
                        let kind = match intent {
                            super::types::OperationIntent::Run { .. } => "run",
                            super::types::OperationIntent::Compaction { .. } => "compaction",
                            super::types::OperationIntent::Navigation { .. } => "navigation",
                        };
                        Some(kind) == query.operation_kind.as_deref()
                    }
                    _ => false,
                }
            })
            && (query.after_seq.is_none() || record.seq() > query.after_seq.unwrap_or_default())
    }
}

fn assign_seq(entry: &mut Entry, seq: u64) {
    match entry {
        Entry::Message { seq: target, .. }
        | Entry::ModelChange { seq: target, .. }
        | Entry::ThinkingLevelChange { seq: target, .. }
        | Entry::ActiveToolsChange { seq: target, .. }
        | Entry::Compaction { seq: target, .. }
        | Entry::BranchSummary { seq: target, .. }
        | Entry::Custom { seq: target, .. } => *target = seq,
    }
}

fn item_seq(item: &LogItem) -> u64 {
    match item {
        LogItem::Entry { seq, .. }
        | LogItem::Record { seq, .. }
        | LogItem::Lane { seq, .. }
        | LogItem::NameFact { seq, .. }
        | LogItem::LabelFact { seq, .. } => *seq,
    }
}
