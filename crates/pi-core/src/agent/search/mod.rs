//! Port of `pi-core/agent/src/search/` (index.ts and scanning.ts).

use std::collections::HashSet;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::agent::harness::session::jsonl::types::JsonlSessionMetadata;
use crate::agent::harness::session::types::{
    Entry, EntryCursor, EntryOrder, EntryQuery, EntryType, SessionError,
};

/// Port of `SessionSearchHit`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
}

/// Port of `SessionSearchOptions`.
#[derive(Clone, Debug, Default)]
pub struct SessionSearchOptions {
    pub entry_types: Option<Vec<EntryType>>,
    pub limit: Option<usize>,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// Port of `ScanningSessionSearchHit`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanningSessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: i64,
    pub snippet: String,
}

/// Port of `SessionSearch`. Errors carry the TypeScript throw messages
/// (for example duplicate session ids from a misbehaving source).
pub trait SessionSearch: Send + Sync {
    fn search<'a>(
        &'a self,
        text: &'a str,
        options: Option<SessionSearchOptions>,
    ) -> BoxFuture<'a, Result<Vec<ScanningSessionSearchHit>, String>>;
}

/// Port of `SessionSearchCandidate`.
#[derive(Clone, Debug)]
pub struct SessionSearchCandidate {
    pub entry_id: String,
    pub seq: u64,
    pub entry_type: EntryType,
    pub timestamp: i64,
    pub text: String,
    pub fields: Option<serde_json::Value>,
}

/// The read surface the scanner needs from a session storage
/// (`ScanningReadable`).
pub trait ScanningReadable: Send + Sync {
    fn get_metadata(&self) -> BoxFuture<'static, JsonlSessionMetadata>;
    fn find_entries(
        &self,
        query: EntryQuery,
    ) -> BoxFuture<'static, Result<Vec<Entry>, SessionError>>;
    fn get_label(&self, id: String) -> BoxFuture<'static, Option<String>>;
}

/// Adapter implementing [`ScanningReadable`] over the storage trait; the
/// JSONL-specific metadata fields are not part of the base trait surface,
/// so identity fields project and the extras stay empty.
pub struct StorageReadable(pub Arc<dyn crate::agent::harness::session::types::SessionStorage>);

impl ScanningReadable for StorageReadable {
    fn get_metadata(&self) -> BoxFuture<'static, JsonlSessionMetadata> {
        let storage = Arc::clone(&self.0);
        Box::pin(async move {
            let base = storage.get_metadata().await;
            JsonlSessionMetadata {
                id: base.id,
                created_at: base.created_at,
                cwd: String::new(),
                path: String::new(),
                modified_at: 0.0,
                source_format: 4,
                parent_session_id: base.parent_session_id,
                legacy_parent_session_path: None,
                metadata: None,
            }
        })
    }

    fn find_entries(
        &self,
        query: EntryQuery,
    ) -> BoxFuture<'static, Result<Vec<Entry>, SessionError>> {
        let storage = Arc::clone(&self.0);
        Box::pin(async move { Ok(storage.find_entries(query).await) })
    }

    fn get_label(&self, id: String) -> BoxFuture<'static, Option<String>> {
        let storage = Arc::clone(&self.0);
        Box::pin(async move { storage.get_label(id).await })
    }
}

/// Port of `ScanningReadableSource`: yields readables for scanning.
pub type ScanningReadableSource = Arc<
    dyn Fn(Option<serde_json::Value>) -> BoxFuture<'static, Vec<Arc<dyn ScanningReadable>>>
        + Send
        + Sync,
>;

/// Port of `ScanningSearchTextProjector`.
pub type ScanningSearchTextProjector =
    Arc<dyn Fn(&JsonlSessionMetadata, &Entry, Option<&str>) -> String + Send + Sync>;

/// Port of `ScanningReadableOptions`.
#[derive(Clone, Default)]
pub struct ScanningReadableOptions {
    pub project_text: Option<ScanningSearchTextProjector>,
    pub page_size: Option<usize>,
}

/// Port of `ScanningSessionSearchOptions`.
#[derive(Clone, Default)]
pub struct ScanningSessionSearchOptions {
    pub base: ScanningReadableOptions,
    pub source_options: Option<SourceOptionsFn>,
    pub matcher: Option<MatchFn>,
    pub create_hit: Option<CreateHitFn>,
}

/// The `sourceOptions` callback.
pub type SourceOptionsFn =
    Arc<dyn Fn(&str, &SessionSearchOptions) -> Option<serde_json::Value> + Send + Sync>;

/// The `match` override.
pub type MatchFn =
    Arc<dyn Fn(&str, &SessionSearchCandidate, &JsonlSessionMetadata) -> bool + Send + Sync>;

/// The `createHit` override.
pub type CreateHitFn = Arc<
    dyn Fn(&JsonlSessionMetadata, &SessionSearchCandidate) -> ScanningSessionSearchHit
        + Send
        + Sync,
>;

fn default_search_text(
    _metadata: &JsonlSessionMetadata,
    entry: &Entry,
    label: Option<&str>,
) -> String {
    let serialized = serde_json::to_string(entry).unwrap_or_default();
    match label {
        None => serialized,
        Some(label) => format!("{serialized} {label}"),
    }
}

async fn scan_readable_entries(
    readable: &Arc<dyn ScanningReadable>,
    metadata: &JsonlSessionMetadata,
    options: &ScanningReadableOptions,
    query_entry_types: Option<&[EntryType]>,
    query_limit: Option<usize>,
) -> Vec<SessionSearchCandidate> {
    let project_text = options
        .project_text
        .clone()
        .unwrap_or_else(|| Arc::new(default_search_text));
    let page_size = query_limit.or(options.page_size).unwrap_or(100);
    let mut after_seq: u64 = 0;
    let mut candidates: Vec<SessionSearchCandidate> = Vec::new();
    loop {
        let mut query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            limit: Some(page_size),
            cursor: Some(EntryCursor { after_seq }),
            ..Default::default()
        };
        if let Some(entry_types) = query_entry_types
            && entry_types.len() == 1
        {
            query.entry_type = Some(entry_types[0]);
        }
        let Ok(entries) = readable.find_entries(query).await else {
            break;
        };
        if entries.is_empty() {
            break;
        }
        for entry in entries {
            if let Some(entry_types) = query_entry_types
                && !entry_types.contains(&entry.entry_type())
            {
                continue;
            }
            let label = readable.get_label(entry.id().to_string()).await;
            candidates.push(SessionSearchCandidate {
                entry_id: entry.id().to_string(),
                seq: entry.seq(),
                entry_type: entry.entry_type(),
                timestamp: entry.timestamp(),
                text: project_text(metadata, &entry, label.as_deref()),
                fields: label.map(|label| serde_json::json!({ "label": label })),
            });
        }
        let last_seq = candidates.last().map(|candidate| candidate.seq);
        match last_seq {
            Some(seq) if seq > after_seq => after_seq = seq,
            _ => break,
        }
        if candidates.len() < page_size {
            break;
        }
    }
    candidates
}

/// Port of `scanningEntries`: scans one readable's full log.
pub async fn scanning_entries(
    readable: &Arc<dyn ScanningReadable>,
    options: &ScanningReadableOptions,
) -> Vec<SessionSearchCandidate> {
    let metadata = readable.get_metadata().await;
    scan_readable_entries(readable, &metadata, options, None, None).await
}

fn default_match(query_text: &str, candidate: &SessionSearchCandidate) -> bool {
    candidate.text.to_lowercase().contains(query_text)
}

fn create_default_scanning_hit(
    metadata: &JsonlSessionMetadata,
    candidate: &SessionSearchCandidate,
) -> ScanningSessionSearchHit {
    ScanningSessionSearchHit {
        session_id: metadata.id.clone(),
        entry_id: candidate.entry_id.clone(),
        timestamp: candidate.timestamp,
        snippet: candidate.text.clone(),
    }
}

/// Port of `createScanningSessionSearch`.
pub fn create_scanning_session_search(
    source: ScanningReadableSource,
    options: ScanningSessionSearchOptions,
) -> Arc<dyn SessionSearch> {
    Arc::new(ScanningSessionSearch { source, options })
}

struct ScanningSessionSearch {
    source: ScanningReadableSource,
    options: ScanningSessionSearchOptions,
}

impl SessionSearch for ScanningSessionSearch {
    fn search<'a>(
        &'a self,
        text: &'a str,
        search_options: Option<SessionSearchOptions>,
    ) -> BoxFuture<'a, Result<Vec<ScanningSessionSearchHit>, String>> {
        Box::pin(async move {
            let search_options = search_options.unwrap_or_default();
            let normalized_text = text.trim().to_lowercase();
            if normalized_text.is_empty() || search_options.limit.is_some_and(|limit| limit == 0) {
                return Ok(Vec::new());
            }
            if search_options
                .entry_types
                .as_ref()
                .is_some_and(|types| types.is_empty())
            {
                return Ok(Vec::new());
            }
            let mut hits: Vec<ScanningSessionSearchHit> = Vec::new();
            let mut seen_session_ids: HashSet<String> = HashSet::new();
            let entry_types = search_options.entry_types.clone();

            let source_options = self
                .options
                .source_options
                .as_ref()
                .and_then(|source_options| source_options(&normalized_text, &search_options));
            let readables = (self.source)(source_options).await;
            for readable in readables {
                if search_options
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_cancelled())
                {
                    return Err("The operation was aborted".to_string());
                }
                let metadata = readable.get_metadata().await;
                if seen_session_ids.contains(&metadata.id) {
                    return Err(format!("Duplicate sessionId: {}", metadata.id));
                }
                seen_session_ids.insert(metadata.id.clone());
                let candidates = scan_readable_entries(
                    &readable,
                    &metadata,
                    &self.options.base,
                    entry_types.as_deref(),
                    None,
                )
                .await;
                for candidate in candidates {
                    if search_options
                        .signal
                        .as_ref()
                        .is_some_and(|signal| signal.is_cancelled())
                    {
                        return Err("The operation was aborted".to_string());
                    }
                    if let Some(entry_types) = &entry_types
                        && !entry_types.contains(&candidate.entry_type)
                    {
                        continue;
                    }
                    let matched = match &self.options.matcher {
                        Some(matcher) => matcher(&normalized_text, &candidate, &metadata),
                        None => default_match(&normalized_text, &candidate),
                    };
                    if !matched {
                        continue;
                    }
                    let hit = match &self.options.create_hit {
                        Some(create_hit) => create_hit(&metadata, &candidate),
                        None => create_default_scanning_hit(&metadata, &candidate),
                    };
                    hits.push(hit);
                    if search_options
                        .limit
                        .is_some_and(|limit| hits.len() >= limit)
                    {
                        return Ok(hits);
                    }
                }
            }
            Ok(hits)
        })
    }
}
