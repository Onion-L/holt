//! Port of `pi-core/agent/src/harness/session/testing/` — the runner-
//! independent session backend conformance cases.
//!
//! `createSessionBackendConformance` builds the shared case list; tests
//! iterate the cases and run them against their backend (in-memory, JSONL).
//! Every case derives a fresh fixture through
//! [`SessionBackendFixture::fresh`], mirroring the TypeScript fixture
//! factory, so cases stay isolated. The assertions mirror the `node:assert`
//! calls in the TypeScript suite; panics carry the case name through the
//! runner.
//!
//! Two TypeScript cases are not portable and are marked below where they
//! would appear:
//!
//! - "rejects non-JSON entries before storage mutation"
//! - "rejects non-JSON records before storage mutation"
//!
//! Both rely on `assertJsonSerializable` rejecting loosely typed JavaScript
//! payloads (circular references, `undefined`, `BigInt`, `NaN`, `Map`).
//! The Rust `Entry`/`LaneRecord` types are strongly typed over JSON values,
//! so those inputs cannot be constructed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use super::jsonl::repo::JsonlSessionRepo;
use super::jsonl::types::JsonlSessionCreateOptions;
use super::jsonl::types::JsonlSessionListOptions;
use super::jsonl::types::JsonlSessionMetadata;
use super::jsonl::types::JsonlSessionRepoOptions;
use super::memory::{InMemorySessionRepo, Session};
use super::types::{
    BranchBounds, Entry, EntryCursor, EntryOrder, EntryQuery, EntryType, ForkOptions, ForkPosition,
    LanePointer, LaneRecord, LogItem, LogOptions, OperationIntent, RecordQuery, RecordType,
    SessionCreateOptions, SessionError, SessionErrorCode, SessionMetadata, SessionStats,
    UsageCause,
};
use crate::agent::harness::env::nodejs::NodeExecutionEnv;
use crate::agent::harness::types::FileSystem;
use crate::agent::types::AgentMessage;
use crate::ai::types::{
    AssistantContent, RoleAssistant, RoleUser, TextContent, Usage, UsageCost, UserContent,
    UserMessage,
};

/// Port of `SessionBackendConformanceCase`.
pub struct SessionBackendConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    #[allow(clippy::type_complexity)]
    pub run: Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

/// Port of `SessionBackendFixture`: a backend instance owned by one
/// conformance case. [`SessionBackendFixture::fresh`] reproduces the
/// TypeScript fixture factory so every case runs against its own isolated
/// backend.
pub enum SessionBackendFixture {
    InMemory(InMemorySessionRepo),
    Jsonl(JsonlSessionFixture),
}

/// Port of `SessionBackendFixtureFactory`.
pub type SessionBackendFixtureFactory =
    Arc<dyn Fn() -> BoxFuture<'static, SessionBackendFixture> + Send + Sync>;

/// The JSONL fixture keeps the filesystem and sessions base so each fresh
/// fixture derives its own sessions root.
pub struct JsonlSessionFixture {
    repo: JsonlSessionRepo,
    fs: Arc<dyn FileSystem>,
    sessions_base: String,
}

/// Repo-level session metadata, one variant per backend fixture.
#[derive(Clone, Debug)]
pub enum FixtureSessionMetadata {
    InMemory(SessionMetadata),
    Jsonl(JsonlSessionMetadata),
}

impl FixtureSessionMetadata {
    fn id(&self) -> &str {
        match self {
            FixtureSessionMetadata::InMemory(metadata) => &metadata.id,
            FixtureSessionMetadata::Jsonl(metadata) => &metadata.id,
        }
    }

    fn created_at(&self) -> i64 {
        match self {
            FixtureSessionMetadata::InMemory(metadata) => metadata.created_at,
            FixtureSessionMetadata::Jsonl(metadata) => metadata.created_at,
        }
    }

    fn parent_session_id(&self) -> Option<&str> {
        match self {
            FixtureSessionMetadata::InMemory(metadata) => metadata.parent_session_id.as_deref(),
            FixtureSessionMetadata::Jsonl(metadata) => metadata.parent_session_id.as_deref(),
        }
    }
}

impl SessionBackendFixture {
    /// Creates the isolated backend one conformance case runs against (the
    /// Rust counterpart of the TypeScript fixture factory).
    pub fn fresh(&self) -> SessionBackendFixture {
        match self {
            SessionBackendFixture::InMemory(_) => {
                SessionBackendFixture::InMemory(InMemorySessionRepo::new())
            }
            SessionBackendFixture::Jsonl(fixture) => {
                static CASE_ROOTS: AtomicU64 = AtomicU64::new(0);
                let case_root = CASE_ROOTS.fetch_add(1, Ordering::Relaxed);
                SessionBackendFixture::Jsonl(JsonlSessionFixture {
                    repo: JsonlSessionRepo::new(JsonlSessionRepoOptions {
                        fs: Arc::clone(&fixture.fs),
                        sessions_root: format!("{}/case-{}", fixture.sessions_base, case_root),
                    }),
                    fs: Arc::clone(&fixture.fs),
                    sessions_base: fixture.sessions_base.clone(),
                })
            }
        }
    }

    fn jsonl_create_options(id: &str) -> JsonlSessionCreateOptions {
        JsonlSessionCreateOptions {
            id: Some(id.to_string()),
            parent_session_id: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            metadata: None,
        }
    }

    async fn create(&self, id: &str) -> Result<Session, SessionError> {
        match self {
            SessionBackendFixture::InMemory(repo) => {
                repo.create(SessionCreateOptions {
                    id: Some(id.to_string()),
                    parent_session_id: None,
                })
                .await
            }
            SessionBackendFixture::Jsonl(fixture) => {
                fixture.repo.create(Self::jsonl_create_options(id)).await
            }
        }
    }

    /// The repo-level metadata for an open session (`getMetadata` plus the
    /// backend-specific fields the JSONL repo needs to reopen).
    async fn session_metadata(&self, session: &Session) -> FixtureSessionMetadata {
        let base = session.get_metadata().await;
        match self {
            SessionBackendFixture::InMemory(_) => FixtureSessionMetadata::InMemory(base),
            SessionBackendFixture::Jsonl(fixture) => {
                let listed = fixture
                    .repo
                    .list(&JsonlSessionListOptions::default())
                    .await
                    .expect("list sessions");
                FixtureSessionMetadata::Jsonl(
                    listed
                        .into_iter()
                        .find(|metadata| metadata.id == base.id)
                        .expect("listed session"),
                )
            }
        }
    }

    async fn open(&self, metadata: &FixtureSessionMetadata) -> Result<Session, SessionError> {
        match (self, metadata) {
            (SessionBackendFixture::InMemory(repo), FixtureSessionMetadata::InMemory(metadata)) => {
                repo.open(metadata.clone()).await
            }
            (SessionBackendFixture::Jsonl(fixture), FixtureSessionMetadata::Jsonl(metadata)) => {
                fixture.repo.open(metadata.clone()).await
            }
            _ => unreachable!("backend and metadata variants must match"),
        }
    }

    async fn list(&self) -> Vec<FixtureSessionMetadata> {
        match self {
            SessionBackendFixture::InMemory(repo) => repo
                .list()
                .await
                .into_iter()
                .map(FixtureSessionMetadata::InMemory)
                .collect(),
            SessionBackendFixture::Jsonl(fixture) => fixture
                .repo
                .list(&JsonlSessionListOptions::default())
                .await
                .unwrap_or_default()
                .into_iter()
                .map(FixtureSessionMetadata::Jsonl)
                .collect(),
        }
    }

    async fn delete(&self, metadata: &FixtureSessionMetadata) -> Result<(), SessionError> {
        match (self, metadata) {
            (SessionBackendFixture::InMemory(repo), FixtureSessionMetadata::InMemory(metadata)) => {
                repo.delete(metadata.clone()).await;
                Ok(())
            }
            (SessionBackendFixture::Jsonl(fixture), FixtureSessionMetadata::Jsonl(metadata)) => {
                fixture.repo.delete(metadata).await
            }
            _ => unreachable!("backend and metadata variants must match"),
        }
    }

    async fn fork(
        &self,
        metadata: &FixtureSessionMetadata,
        options: ForkOptions,
        id: &str,
    ) -> Result<Session, SessionError> {
        match (self, metadata) {
            (SessionBackendFixture::InMemory(repo), FixtureSessionMetadata::InMemory(metadata)) => {
                repo.fork(
                    metadata.clone(),
                    options,
                    SessionCreateOptions {
                        id: Some(id.to_string()),
                        parent_session_id: None,
                    },
                )
                .await
            }
            (SessionBackendFixture::Jsonl(fixture), FixtureSessionMetadata::Jsonl(metadata)) => {
                fixture
                    .repo
                    .fork(metadata.clone(), options, Self::jsonl_create_options(id))
                    .await
            }
            _ => unreachable!("backend and metadata variants must match"),
        }
    }
}

fn create_user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![crate::ai::types::BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })]),
        timestamp: 1,
    })
}

fn create_assistant_message(text: &str) -> AgentMessage {
    create_assistant_message_with_usage(text, Usage::default())
}

fn create_assistant_message_with_usage(text: &str, usage: Usage) -> AgentMessage {
    AgentMessage::Assistant(Box::new(crate::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage,
        stop_reason: crate::ai::types::StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }))
}

fn operation_started(id: &str, lane: &str, kind: &str) -> LaneRecord {
    let intent = match kind {
        "run" => OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        },
        "compaction" => OperationIntent::Compaction {
            custom_instructions: None,
            result_entry_id: format!("{id}-result"),
        },
        _ => OperationIntent::Navigation {
            target_id: None,
            summarize: false,
            custom_instructions: None,
            label: None,
            summary_entry_id: None,
        },
    };
    LaneRecord::OperationStarted {
        id: id.to_string(),
        lane: lane.to_string(),
        source_leaf_id: None,
        intent,
        seq: 0,
        timestamp: 0,
    }
}

fn operation_finished(id: &str, lane: &str, run_id: &str, outcome: &str) -> LaneRecord {
    LaneRecord::OperationFinished {
        id: id.to_string(),
        lane: lane.to_string(),
        run_id: run_id.to_string(),
        outcome: outcome.to_string(),
        error: None,
        seq: 0,
        timestamp: 0,
    }
}

fn step_attempt(id: &str, lane: &str, run_id: &str, result_entry_id: &str) -> LaneRecord {
    LaneRecord::StepAttempt {
        id: id.to_string(),
        lane: lane.to_string(),
        run_id: run_id.to_string(),
        step: "assistant".to_string(),
        attempt: 1,
        result_entry_id: result_entry_id.to_string(),
        compaction_reason: None,
        seq: 0,
        timestamp: 0,
    }
}

fn queue_enqueued(id: &str, lane: &str, target: Entry) -> LaneRecord {
    LaneRecord::QueueEnqueued {
        id: id.to_string(),
        lane: lane.to_string(),
        queue: "nextRun".to_string(),
        run_id: None,
        target,
        seq: 0,
        timestamp: 0,
    }
}

fn queue_cancelled(id: &str, lane: &str, entry_id: &str) -> LaneRecord {
    LaneRecord::QueueCancelled {
        id: id.to_string(),
        lane: lane.to_string(),
        run_id: None,
        entry_id: entry_id.to_string(),
        seq: 0,
        timestamp: 0,
    }
}

fn usage_record(id: &str, lane: &str, usage: Usage, cause: UsageCause) -> LaneRecord {
    LaneRecord::Usage {
        id: id.to_string(),
        lane: lane.to_string(),
        usage,
        cause,
        seq: 0,
        timestamp: 0,
    }
}

fn message_entry(id: &str, message: AgentMessage) -> Entry {
    Entry::Message {
        id: id.to_string(),
        message,
        terminate: None,
        parent_id: None,
        seq: 0,
        timestamp: 0,
    }
}

fn custom_entry(id: &str, custom_type: &str, data: Option<serde_json::Value>) -> Entry {
    Entry::Custom {
        id: id.to_string(),
        custom_type: custom_type.to_string(),
        data,
        parent_id: None,
        seq: 0,
        timestamp: 0,
    }
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn entry_ids(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

fn record_ids(records: &[LaneRecord]) -> Vec<String> {
    records
        .iter()
        .map(|record| record.id().to_string())
        .collect()
}

fn log_item_kind(item: &LogItem) -> &'static str {
    match item {
        LogItem::Entry { .. } => "entry",
        LogItem::Record { .. } => "record",
        LogItem::Lane { .. } => "lane",
        LogItem::NameFact { .. } | LogItem::LabelFact { .. } => "fact",
    }
}

fn log_item_seq(item: &LogItem) -> u64 {
    match item {
        LogItem::Entry { seq, .. }
        | LogItem::Record { seq, .. }
        | LogItem::Lane { seq, .. }
        | LogItem::NameFact { seq, .. }
        | LogItem::LabelFact { seq, .. } => *seq,
    }
}

/// The log as `(kind, seq)` pairs; both fact variants map to `"fact"`.
fn log_kinds(log: &[LogItem]) -> Vec<(&'static str, u64)> {
    log.iter()
        .map(|item| (log_item_kind(item), log_item_seq(item)))
        .collect()
}

fn log_seqs(log: &[LogItem]) -> Vec<u64> {
    log.iter().map(log_item_seq).collect()
}

fn usage_cause(record: &LaneRecord) -> &'static str {
    match record {
        LaneRecord::Usage { cause, .. } => match cause {
            UsageCause::Assistant { .. } => "assistant",
            UsageCause::Compaction { .. } => "compaction",
            UsageCause::BranchSummary { .. } => "branch_summary",
            UsageCause::DeferredFetch { .. } => "deferred_fetch",
            UsageCause::Tool { .. } => "tool",
            UsageCause::Hook { .. } => "hook",
            UsageCause::Adjustment { .. } => "adjustment",
        },
        _ => panic!("expected usage record"),
    }
}

/// Port of `rejectsWithCode`.
fn rejects_with_code<T>(result: Result<T, SessionError>, code: SessionErrorCode) {
    match result {
        Ok(_) => panic!("expected SessionError with code {code:?}"),
        Err(error) => assert_eq!(
            error.code, code,
            "expected SessionError with code {code:?}, got {error}"
        ),
    }
}

type ConformanceTest = fn(SessionBackendFixture) -> BoxFuture<'static, ()>;

/// Port of `createCase`: registers one case that runs against a freshly
/// created fixture.
fn case(
    cases: &mut Vec<SessionBackendConformanceCase>,
    fixture: &Arc<SessionBackendFixture>,
    group: &'static str,
    name: &'static str,
    test: ConformanceTest,
) {
    let fixture = Arc::clone(fixture);
    cases.push(SessionBackendConformanceCase {
        group,
        name,
        run: Box::new(move || {
            let fixture = Arc::clone(&fixture);
            Box::pin(async move {
                let fixture = fixture.fresh();
                test(fixture).await;
            })
        }),
    });
}

/// Port of `createSessionBackendConformance`: the shared backend
/// conformance cases. Each case constructs a fresh fixture, so cases stay
/// isolated.
pub fn create_session_backend_conformance(
    fixture: Arc<SessionBackendFixture>,
) -> Vec<SessionBackendConformanceCase> {
    let mut cases: Vec<SessionBackendConformanceCase> = Vec::new();

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "assigns parents and one sequence across every mutation",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let root = session
                    .append_entry(message_entry("root", create_user_message("root")), "main")
                    .await
                    .expect("root");
                session
                    .create_lane("thread", Some(root.id().to_string()))
                    .await
                    .expect("lane");
                let child = session
                    .append_entry(
                        custom_entry("child", "note", Some(serde_json::json!({ "value": 1 }))),
                        "thread",
                    )
                    .await
                    .expect("child");
                let record = session
                    .append_record(operation_started("run", "thread", "run"))
                    .await
                    .expect("record");
                session
                    .set_name(Some("Example".to_string()))
                    .await
                    .expect("name");
                session
                    .set_label(root.id(), Some("checkpoint".to_string()))
                    .await
                    .expect("label");
                session
                    .move_lane("main", Some(child.id().to_string()))
                    .await
                    .expect("move");

                assert_eq!(root.parent_id(), None);
                assert_eq!(root.seq(), 1);
                assert_eq!(child.parent_id(), Some("root"));
                assert_eq!(child.seq(), 3);
                assert_eq!(record.seq(), 4);
                for timestamp in [root.timestamp(), child.timestamp(), record.timestamp()] {
                    assert!(
                        timestamp >= 0,
                        "storage-assigned timestamps must be Unix milliseconds"
                    );
                }
                let log = session.get_log(LogOptions::default()).await.expect("log");
                assert_eq!(
                    log_kinds(&log),
                    vec![
                        ("entry", 1),
                        ("lane", 2),
                        ("entry", 3),
                        ("record", 4),
                        ("fact", 5),
                        ("fact", 6),
                        ("lane", 7),
                    ]
                );
                assert_eq!(
                    session.get_lanes().await,
                    vec![
                        LanePointer {
                            lane: "main".to_string(),
                            leaf_id: Some("child".to_string()),
                        },
                        LanePointer {
                            lane: "thread".to_string(),
                            leaf_id: Some("child".to_string()),
                        },
                    ]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "commits records and lane moves as separate mutations",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let root = session
                    .append_entry(message_entry("root", create_user_message("root")), "main")
                    .await
                    .expect("root");
                let finished = session
                    .append_record(operation_finished("finish", "main", "run", "completed"))
                    .await
                    .expect("record");

                assert_eq!(finished.seq(), 2);
                assert_eq!(
                    session.get_lanes().await,
                    vec![LanePointer {
                        lane: "main".to_string(),
                        leaf_id: Some("root".to_string()),
                    }]
                );
                session.move_lane("main", None).await.expect("move");
                assert_eq!(
                    session.get_lanes().await,
                    vec![LanePointer {
                        lane: "main".to_string(),
                        leaf_id: None,
                    }]
                );
                assert_eq!(
                    session.get_log(LogOptions::default()).await.expect("log"),
                    vec![
                        LogItem::Entry {
                            seq: 1,
                            entry: root.clone(),
                        },
                        LogItem::Record {
                            seq: 2,
                            record: finished.clone(),
                        },
                        LogItem::Lane {
                            seq: 3,
                            lane: "main".to_string(),
                            leaf_id: None,
                        },
                    ]
                );

                rejects_with_code(
                    session.move_lane("main", Some("missing".to_string())).await,
                    SessionErrorCode::NotFound,
                );
                assert_eq!(
                    session
                        .find_records(RecordQuery::default())
                        .await
                        .expect("find records")
                        .len(),
                    1
                );
                let log = session.get_log(LogOptions::default()).await.expect("log");
                assert_eq!(log_seqs(&log), [1, 2, 3]);
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "rejects duplicate ids without changing state",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_entry(message_entry("shared", create_user_message("root")), "main")
                    .await
                    .expect("root");
                rejects_with_code(
                    session
                        .append_record(operation_started("shared", "main", "run"))
                        .await,
                    SessionErrorCode::AlreadyExists,
                );
                session
                    .append_record(operation_started("run", "main", "run"))
                    .await
                    .expect("record");
                rejects_with_code(
                    session
                        .append_entry(custom_entry("run", "note", None), "main")
                        .await,
                    SessionErrorCode::AlreadyExists,
                );
                let log = session.get_log(LogOptions::default()).await.expect("log");
                assert_eq!(log_seqs(&log), [1, 2]);
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "isolates lanes while sharing the tree",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_entry(message_entry("root", create_user_message("root")), "main")
                    .await
                    .expect("root");
                session
                    .create_lane("thread", Some("root".to_string()))
                    .await
                    .expect("lane");
                session
                    .append_entry(
                        message_entry("main-child", create_user_message("main")),
                        "main",
                    )
                    .await
                    .expect("main append");
                session
                    .append_entry(
                        message_entry("thread-child", create_user_message("thread")),
                        "thread",
                    )
                    .await
                    .expect("thread append");

                assert_eq!(
                    session.get_lanes().await,
                    vec![
                        LanePointer {
                            lane: "main".to_string(),
                            leaf_id: Some("main-child".to_string()),
                        },
                        LanePointer {
                            lane: "thread".to_string(),
                            leaf_id: Some("thread-child".to_string()),
                        },
                    ]
                );
                let main_branch = session
                    .find_entries_on_branch(
                        EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        BranchBounds {
                            start: Some("main-child".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("main branch");
                assert_eq!(entry_ids(&main_branch), ids(&["root", "main-child"]));
                let thread_branch = session
                    .find_entries_on_branch(
                        EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        BranchBounds {
                            start: Some("thread-child".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("thread branch");
                assert_eq!(entry_ids(&thread_branch), ids(&["root", "thread-child"]));
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "queries and facts",
        "rejects invalid queries before empty reads",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("invalid-queries").await.expect("create");
                session.create_lane("thread", None).await.expect("lane");
                let thread = session.view("thread");

                // Zero limits are invalid even before any read happens. The
                // TypeScript suite additionally rejects negative limits and
                // `cursor: { afterSeq: -1 }`; `usize` limits and `u64`
                // cursors cannot express those values (see the note on
                // `assert_valid_limit` in memory.rs).
                let zero_limit_entry = EntryQuery {
                    limit: Some(0),
                    ..Default::default()
                };
                rejects_with_code(
                    session.find_entries(zero_limit_entry.clone()).await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session.find_entry(zero_limit_entry.clone()).await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .find_entries_on_branch(zero_limit_entry.clone(), BranchBounds::default())
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    thread
                        .find_entries_on_branch(zero_limit_entry.clone(), BranchBounds::default())
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    thread
                        .find_entry_on_branch(zero_limit_entry, BranchBounds::default())
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .find_records(RecordQuery {
                            limit: Some(0),
                            ..Default::default()
                        })
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .find_records(RecordQuery {
                            operation_kind: Some("run".to_string()),
                            ..Default::default()
                        })
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .find_records(RecordQuery {
                            record_type: Some(RecordType::StepAttempt),
                            operation_kind: Some("run".to_string()),
                            ..Default::default()
                        })
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session.find_open_operations("main", Some(0)).await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .get_log(LogOptions {
                            limit: Some(0),
                            ..Default::default()
                        })
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "queries and facts",
        "supports bounded filtered and cursor-based queries",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_entry(message_entry("root", create_user_message("root")), "main")
                    .await
                    .expect("root");
                session
                    .append_entry(
                        custom_entry("old-note", "note", Some(serde_json::json!(1))),
                        "main",
                    )
                    .await
                    .expect("old note");
                session
                    .append_entry(
                        Entry::Compaction {
                            id: "compact".to_string(),
                            summary: "summary".to_string(),
                            retained_tail: Vec::new(),
                            tokens_before: 10,
                            details: None,
                            usage: None,
                            parent_id: None,
                            seq: 0,
                            timestamp: 0,
                        },
                        "main",
                    )
                    .await
                    .expect("compaction");
                session
                    .append_entry(
                        custom_entry("new-note", "note", Some(serde_json::json!(2))),
                        "main",
                    )
                    .await
                    .expect("new note");
                session
                    .append_entry(
                        message_entry("tail", create_assistant_message("tail")),
                        "main",
                    )
                    .await
                    .expect("tail");

                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries(EntryQuery::default())
                            .await
                            .expect("find")
                    ),
                    ids(&["tail", "new-note", "compact", "old-note", "root"])
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                cursor: Some(EntryCursor { after_seq: 2 }),
                                limit: Some(2),
                                ..Default::default()
                            })
                            .await
                            .expect("cursor query")
                    ),
                    ids(&["compact", "new-note"])
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries(EntryQuery {
                                custom_type: Some("note".to_string()),
                                ..Default::default()
                            })
                            .await
                            .expect("customType query")
                    ),
                    ids(&["new-note", "old-note"])
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries_on_branch(
                                EntryQuery {
                                    custom_type: Some("note".to_string()),
                                    limit: Some(1),
                                    ..Default::default()
                                },
                                BranchBounds {
                                    start: Some("tail".to_string()),
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("branch note query")
                    ),
                    ids(&["new-note"])
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries_on_branch(
                                EntryQuery {
                                    entry_type: Some(EntryType::Message),
                                    ..Default::default()
                                },
                                BranchBounds {
                                    start: Some("tail".to_string()),
                                    stop_at_type: Some(EntryType::Compaction),
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("stopAtType query")
                    ),
                    ids(&["tail"])
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries_on_branch(
                                EntryQuery {
                                    entry_type: Some(EntryType::Custom),
                                    ..Default::default()
                                },
                                BranchBounds {
                                    start: Some("tail".to_string()),
                                    stop_at_id: Some("tail".to_string()),
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("stopAtId query")
                    ),
                    Vec::<String>::new()
                );
                assert_eq!(
                    entry_ids(
                        &session
                            .find_entries_on_branch(
                                EntryQuery {
                                    order: Some(EntryOrder::OldestFirst),
                                    ..Default::default()
                                },
                                BranchBounds {
                                    start: Some("tail".to_string()),
                                    stop_at_type: Some(EntryType::Custom),
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("oldestFirst stop query")
                    ),
                    ids(&["root", "old-note"])
                );
                rejects_with_code(
                    session
                        .find_entries(EntryQuery {
                            limit: Some(0),
                            ..Default::default()
                        })
                        .await,
                    SessionErrorCode::InvalidQuery,
                );
                rejects_with_code(
                    session
                        .find_entries_on_branch(
                            EntryQuery::default(),
                            BranchBounds {
                                start: Some("missing".to_string()),
                                ..Default::default()
                            },
                        )
                        .await,
                    SessionErrorCode::NotFound,
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "keeps lane names permanent with their recovery records",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session.create_lane("thread", None).await.expect("lane");
                session
                    .append_record(operation_started("old-run", "thread", "run"))
                    .await
                    .expect("old run");
                session
                    .append_record(queue_enqueued(
                        "old-next-run",
                        "thread",
                        message_entry("queued-message", create_user_message("queued")),
                    ))
                    .await
                    .expect("queued run");

                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                lane: Some("thread".to_string()),
                                ..Default::default()
                            })
                            .await
                            .expect("lane records")
                    ),
                    ids(&["old-next-run", "old-run"])
                );
                let log = session.get_log(LogOptions::default()).await.expect("log");
                let log_record_ids: Vec<String> = log
                    .iter()
                    .filter_map(|item| match item {
                        LogItem::Record { record, .. } => Some(record.id().to_string()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(log_record_ids, ids(&["old-run", "old-next-run"]));
                rejects_with_code(
                    session.create_lane("thread", None).await,
                    SessionErrorCode::AlreadyExists,
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "persists queue cancellation without consuming its target",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let enqueued = session
                    .append_record(queue_enqueued(
                        "enqueue",
                        "main",
                        message_entry("queued-message", create_user_message("queued")),
                    ))
                    .await
                    .expect("enqueue");
                let cancelled = session
                    .append_record(queue_cancelled("cancel", "main", "queued-message"))
                    .await
                    .expect("cancel");
                assert_eq!(cancelled.seq(), 2);
                match &cancelled {
                    LaneRecord::QueueCancelled {
                        run_id: None,
                        entry_id,
                        ..
                    } => assert_eq!(entry_id, "queued-message"),
                    _ => panic!("expected a queue cancellation without runId"),
                }
                assert!(session.get_entry("queued-message").await.is_none());
                let cancellations = session
                    .find_records(RecordQuery {
                        record_type: Some(RecordType::QueueCancelled),
                        ..Default::default()
                    })
                    .await
                    .expect("cancellations");
                assert_eq!(cancellations, vec![cancelled.clone()]);
                assert_eq!(
                    session.get_log(LogOptions::default()).await.expect("log"),
                    vec![
                        LogItem::Record {
                            seq: enqueued.seq(),
                            record: enqueued.clone(),
                        },
                        LogItem::Record {
                            seq: cancelled.seq(),
                            record: cancelled.clone(),
                        },
                    ]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "filters records by lane type run sequence and order",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_record(operation_started("run-1", "main", "run"))
                    .await
                    .expect("run-1");
                session
                    .append_record(step_attempt("attempt-1", "main", "run-1", "assistant-1"))
                    .await
                    .expect("attempt-1");
                session.create_lane("thread", None).await.expect("lane");
                session
                    .append_record(operation_started("run-2", "thread", "run"))
                    .await
                    .expect("run-2");
                session
                    .append_record(step_attempt("attempt-2", "thread", "run-2", "assistant-2"))
                    .await
                    .expect("attempt-2");

                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                lane: Some("thread".to_string()),
                                ..Default::default()
                            })
                            .await
                            .expect("lane filter")
                    ),
                    ids(&["attempt-2", "run-2"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                record_type: Some(RecordType::StepAttempt),
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("type filter")
                    ),
                    ids(&["attempt-1", "attempt-2"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                run_id: Some("run-1".to_string()),
                                after_seq: Some(1),
                                ..Default::default()
                            })
                            .await
                            .expect("run filter")
                    ),
                    ids(&["attempt-1"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                limit: Some(1),
                                ..Default::default()
                            })
                            .await
                            .expect("limit filter")
                    ),
                    ids(&["attempt-2"])
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "filters operation starts by operation kind",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_record(operation_started("run-old", "main", "run"))
                    .await
                    .expect("run-old");
                session
                    .append_record(operation_finished(
                        "run-old-finished",
                        "main",
                        "run-old",
                        "completed",
                    ))
                    .await
                    .expect("run-old-finished");
                session
                    .append_record(operation_started("compaction", "main", "compaction"))
                    .await
                    .expect("compaction");
                session
                    .append_record(operation_finished(
                        "compaction-finished",
                        "main",
                        "compaction",
                        "completed",
                    ))
                    .await
                    .expect("compaction-finished");
                session
                    .append_record(operation_started("navigation", "main", "navigation"))
                    .await
                    .expect("navigation");
                session
                    .append_record(operation_finished(
                        "navigation-finished",
                        "main",
                        "navigation",
                        "completed",
                    ))
                    .await
                    .expect("navigation-finished");
                session
                    .append_record(operation_started("run-new", "main", "run"))
                    .await
                    .expect("run-new");

                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                record_type: Some(RecordType::OperationStarted),
                                operation_kind: Some("run".to_string()),
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("run starts")
                    ),
                    ids(&["run-old", "run-new"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                record_type: Some(RecordType::OperationStarted),
                                operation_kind: Some("compaction".to_string()),
                                ..Default::default()
                            })
                            .await
                            .expect("compaction starts")
                    ),
                    ids(&["compaction"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                record_type: Some(RecordType::OperationStarted),
                                operation_kind: Some("navigation".to_string()),
                                ..Default::default()
                            })
                            .await
                            .expect("navigation starts")
                    ),
                    ids(&["navigation"])
                );
                assert_eq!(
                    record_ids(
                        &session
                            .find_records(RecordQuery {
                                record_type: Some(RecordType::OperationStarted),
                                operation_kind: Some("run".to_string()),
                                limit: Some(1),
                                ..Default::default()
                            })
                            .await
                            .expect("limited run starts")
                    ),
                    ids(&["run-new"])
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "tracks and enforces one open operation per lane",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                assert_eq!(
                    session
                        .find_open_operations("main", Some(2))
                        .await
                        .expect("open operations"),
                    Vec::<LaneRecord>::new()
                );

                let first = session
                    .append_record(operation_started("first", "main", "run"))
                    .await
                    .expect("first operation");
                assert_eq!(
                    session
                        .find_open_operations("main", Some(2))
                        .await
                        .expect("open operations"),
                    vec![first.clone()]
                );
                rejects_with_code(
                    session
                        .append_record(operation_started("second", "main", "run"))
                        .await,
                    SessionErrorCode::Storage,
                );
                assert_eq!(
                    session
                        .find_open_operations("main", Some(2))
                        .await
                        .expect("open operations"),
                    vec![first.clone()]
                );

                session
                    .append_record(operation_finished(
                        "finish-first",
                        "main",
                        first.id(),
                        "completed",
                    ))
                    .await
                    .expect("finish");
                assert_eq!(
                    session
                        .find_open_operations("main", Some(2))
                        .await
                        .expect("open operations"),
                    Vec::<LaneRecord>::new()
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "does not let an earlier finish close a later start",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_record(operation_finished(
                        "finish-before-start",
                        "main",
                        "run",
                        "completed",
                    ))
                    .await
                    .expect("finish");
                let started = session
                    .append_record(operation_started("run", "main", "run"))
                    .await
                    .expect("start");
                assert_eq!(
                    session
                        .find_open_operations("main", Some(2))
                        .await
                        .expect("open operations"),
                    vec![started.clone()]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "records and log",
        "scopes open operations by lane and limit",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session.create_lane("thread", None).await.expect("lane");
                let main_run = session
                    .append_record(operation_started("main-run", "main", "run"))
                    .await
                    .expect("main run");
                let thread_navigation = session
                    .append_record(operation_started(
                        "thread-navigation",
                        "thread",
                        "navigation",
                    ))
                    .await
                    .expect("thread navigation");

                assert_eq!(
                    session
                        .find_open_operations("main", None)
                        .await
                        .expect("main open operations"),
                    vec![main_run.clone()]
                );
                assert_eq!(
                    session
                        .find_open_operations("main", Some(1))
                        .await
                        .expect("main open operations"),
                    vec![main_run.clone()]
                );
                assert_eq!(
                    session
                        .find_open_operations("thread", Some(2))
                        .await
                        .expect("thread open operations"),
                    vec![thread_navigation.clone()]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "validation and immutability",
        "returns immutable open-operation records",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let committed = session
                    .append_record(operation_started("run", "main", "run"))
                    .await
                    .expect("record");
                let mut read = session
                    .find_open_operations("main", None)
                    .await
                    .expect("open operations")
                    .into_iter()
                    .next()
                    .expect("one open operation");
                match &mut read {
                    LaneRecord::OperationStarted {
                        intent:
                            OperationIntent::Run {
                                original_prompt, ..
                            },
                        ..
                    } => original_prompt.push(create_user_message("mutated")),
                    _ => panic!("expected an open run operation"),
                }

                assert_eq!(
                    session
                        .find_open_operations("main", None)
                        .await
                        .expect("open operations"),
                    vec![committed.clone()]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "queries and facts",
        "keeps latest-value facts and computes ledger statistics across lanes",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let assistant_usage = Usage {
                    input: 10,
                    output: 5,
                    cache_read: 3,
                    cache_write: 2,
                    total_tokens: 20,
                    cost: UsageCost {
                        input: 1.0.into(),
                        output: 2.0.into(),
                        cache_read: 3.0.into(),
                        cache_write: 4.0.into(),
                        total: 10.0.into(),
                    },
                    ..Default::default()
                };
                session
                    .append_entry(
                        message_entry("user", create_user_message("question")),
                        "main",
                    )
                    .await
                    .expect("user");
                session
                    .append_entry(
                        message_entry(
                            "assistant",
                            create_assistant_message_with_usage("answer", assistant_usage.clone()),
                        ),
                        "main",
                    )
                    .await
                    .expect("assistant");
                session
                    .append_record(usage_record(
                        "assistant-usage",
                        "main",
                        assistant_usage,
                        UsageCause::Assistant {
                            run_id: "run".to_string(),
                            entry_id: "assistant".to_string(),
                            attempt: 1,
                            stop_reason: "stop".to_string(),
                        },
                    ))
                    .await
                    .expect("assistant usage");
                session
                    .append_record(usage_record(
                        "deferred-usage",
                        "main",
                        Usage::default(),
                        UsageCause::DeferredFetch {
                            run_id: "run".to_string(),
                            entry_id: "deferred-result".to_string(),
                            attempt: 1,
                            stop_reason: "deferred".to_string(),
                        },
                    ))
                    .await
                    .expect("deferred usage");
                session
                    .create_lane("thread", Some("assistant".to_string()))
                    .await
                    .expect("lane");
                // The TypeScript correction record carries negative token
                // deltas (input: -2, totalTokens: -2) plus a -0.5 cost
                // correction. The Rust `Usage` token counters are unsigned,
                // so only the negative cost correction is representable;
                // the expected token statistics shift accordingly
                // (uncached 10 -> 12, total 18 -> 20) while `costTotal`
                // stays at the TypeScript value of 9.5.
                session
                    .append_record(usage_record(
                        "correction",
                        "thread",
                        Usage {
                            cost: UsageCost {
                                total: (-0.5f64).into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        UsageCause::Adjustment {
                            run_id: None,
                            entry_id: None,
                            details: Some(serde_json::json!({ "reason": "provider correction" })),
                        },
                    ))
                    .await
                    .expect("correction");
                session
                    .set_name(Some("First".to_string()))
                    .await
                    .expect("first name");
                session
                    .set_name(Some("Second".to_string()))
                    .await
                    .expect("second name");
                session
                    .set_label("user", Some("keep".to_string()))
                    .await
                    .expect("label");
                session.set_label("user", None).await.expect("clear label");
                rejects_with_code(
                    session
                        .set_label("missing", Some("checkpoint".to_string()))
                        .await,
                    SessionErrorCode::NotFound,
                );

                assert_eq!(session.get_name().await.as_deref(), Some("Second"));
                assert_eq!(session.get_label("user").await, None);
                let usage_records = session
                    .find_records(RecordQuery {
                        record_type: Some(RecordType::Usage),
                        order: Some(EntryOrder::OldestFirst),
                        ..Default::default()
                    })
                    .await
                    .expect("usage records");
                assert_eq!(
                    usage_records.iter().map(usage_cause).collect::<Vec<_>>(),
                    ["assistant", "deferred_fetch", "adjustment"]
                );
                let deferred_usage = usage_records
                    .iter()
                    .find(|record| usage_cause(record) == "deferred_fetch")
                    .expect("deferred usage record");
                match deferred_usage {
                    LaneRecord::Usage {
                        cause: UsageCause::DeferredFetch { stop_reason, .. },
                        ..
                    } => assert_eq!(stop_reason, "deferred"),
                    _ => panic!("expected a deferred usage record"),
                }
                assert_eq!(
                    session.get_stats().await,
                    SessionStats {
                        message_count: 2,
                        cached_tokens: 3,
                        uncached_tokens: 12,
                        total_tokens: 20,
                        cost_total: 9.5,
                    }
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "queries and facts",
        "clears session names durably",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .set_name(Some("Temporary".to_string()))
                    .await
                    .expect("set");
                session.set_name(None).await.expect("clear");

                assert_eq!(session.get_name().await, None);
                assert_eq!(
                    session.get_log(LogOptions::default()).await.expect("log"),
                    vec![
                        LogItem::NameFact {
                            seq: 1,
                            name: Some("Temporary".to_string()),
                        },
                        LogItem::NameFact { seq: 2, name: None },
                    ]
                );

                let metadata = fixture.session_metadata(&session).await;
                let reopened = fixture.open(&metadata).await.expect("open");
                assert_eq!(reopened.get_name().await, None);
                assert_eq!(
                    reopened.get_log(LogOptions::default()).await.expect("log"),
                    vec![
                        LogItem::NameFact {
                            seq: 1,
                            name: Some("Temporary".to_string()),
                        },
                        LogItem::NameFact { seq: 2, name: None },
                    ]
                );

                let fork = fixture
                    .fork(
                        &metadata,
                        ForkOptions::Branch {
                            entry_id: None,
                            position: None,
                        },
                        "fork",
                    )
                    .await
                    .expect("fork");
                assert_eq!(fork.get_name().await, None);
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "validation and immutability",
        "returns immutable copies from reads",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("immutable").await.expect("create");
                let metadata = session.get_metadata().await;
                let mut data = serde_json::json!({ "nested": { "value": 1 } });
                session
                    .append_entry(custom_entry("custom", "note", Some(data.clone())), "main")
                    .await
                    .expect("append");
                data["nested"]["value"] = serde_json::json!(50);
                let read = session.get_entry("custom").await.expect("read");
                // The TypeScript suite mutates the returned copies and the
                // caller's input object; Rust returns owned clones, so
                // mutating local copies and re-reading covers the same
                // immutability contract.
                let expected_data = serde_json::json!({ "nested": { "value": 1 } });
                match &read {
                    Entry::Custom { data, .. } => assert_eq!(data.as_ref(), Some(&expected_data)),
                    _ => panic!("expected custom entry"),
                }
                let mut mutated_read = read.clone();
                match &mut mutated_read {
                    Entry::Custom {
                        data: Some(serde_json::Value::Object(fields)),
                        ..
                    } => {
                        fields
                            .entry("nested")
                            .or_insert(serde_json::json!({}))
                            .as_object_mut()
                            .expect("nested object")
                            .insert("value".to_string(), serde_json::json!(99));
                    }
                    _ => panic!("expected custom entry"),
                }
                let mut mutated_metadata = metadata.clone();
                mutated_metadata.id = "changed".to_string();
                let mut log = session.get_log(LogOptions::default()).await.expect("log");
                match log.first_mut() {
                    Some(LogItem::Entry {
                        entry:
                            Entry::Custom {
                                data: Some(serde_json::Value::Object(fields)),
                                ..
                            },
                        ..
                    }) => {
                        fields
                            .entry("nested")
                            .or_insert(serde_json::json!({}))
                            .as_object_mut()
                            .expect("nested object")
                            .insert("value".to_string(), serde_json::json!(100));
                    }
                    _ => panic!("expected entry log"),
                }

                assert_eq!(session.get_metadata().await, metadata);
                assert_eq!(
                    session.get_entry("custom").await,
                    Some(Entry::Custom {
                        id: "custom".to_string(),
                        custom_type: "note".to_string(),
                        data: Some(expected_data),
                        parent_id: None,
                        seq: 1,
                        timestamp: read.timestamp(),
                    })
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "validates lane lifecycle and targets",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                rejects_with_code(
                    session.create_lane("main", None).await,
                    SessionErrorCode::AlreadyExists,
                );
                rejects_with_code(
                    session
                        .create_lane("thread", Some("missing".to_string()))
                        .await,
                    SessionErrorCode::NotFound,
                );
                rejects_with_code(
                    session.move_lane("missing", None).await,
                    SessionErrorCode::InvalidLane,
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "binds lane views without caching leaves",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let root = session
                    .append_message(create_user_message("root"))
                    .await
                    .expect("root");
                session
                    .create_lane("thread", Some(root.clone()))
                    .await
                    .expect("lane");
                let thread = session.view("thread");
                let (main_child, thread_child) = tokio::join!(
                    session.append_message(create_user_message("main")),
                    thread.append_message(create_user_message("thread")),
                );
                let main_child = main_child.expect("main append");
                let thread_child = thread_child.expect("thread append");

                assert_eq!(
                    session.get_leaf_id().await.expect("leaf"),
                    Some(main_child.clone())
                );
                assert_eq!(
                    thread.get_leaf_id().await.expect("thread leaf"),
                    Some(thread_child.clone())
                );
                let main_branch = session
                    .find_entries_on_branch(
                        EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        BranchBounds::default(),
                    )
                    .await
                    .expect("main branch");
                assert_eq!(entry_ids(&main_branch), ids(&[&root, &main_child]));
                let thread_branch = thread
                    .find_entries_on_branch(
                        EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        BranchBounds::default(),
                    )
                    .await
                    .expect("thread branch");
                assert_eq!(entry_ids(&thread_branch), ids(&[&root, &thread_child]));
                let empty = fixture.create("empty").await.expect("empty");
                assert!(
                    empty
                        .find_entries_on_branch(EntryQuery::default(), BranchBounds::default())
                        .await
                        .expect("empty branch")
                        .is_empty()
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "appends provisioned entries with their existing ids",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let entry = session
                    .append_entry(
                        custom_entry(
                            "provisioned",
                            "note",
                            Some(serde_json::json!({ "value": 1 })),
                        ),
                        "main",
                    )
                    .await
                    .expect("append");

                match &entry {
                    Entry::Custom {
                        id,
                        custom_type,
                        parent_id,
                        seq,
                        ..
                    } => {
                        assert_eq!(custom_type, "note");
                        assert_eq!(
                            (id.as_str(), parent_id.as_deref(), *seq),
                            ("provisioned", None, 1)
                        );
                    }
                    _ => panic!("expected custom entry"),
                }
                assert_eq!(
                    session.get_leaf_id().await.expect("leaf"),
                    Some("provisioned".to_string())
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "persists tool-result termination decisions",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                let entry = session
                    .append_entry(
                        Entry::Message {
                            id: "tool-result".to_string(),
                            message: AgentMessage::ToolResult(Box::new(
                                crate::ai::types::ToolResultMessage {
                                    role: Default::default(),
                                    tool_call_id: "call-1".to_string(),
                                    tool_name: "example".to_string(),
                                    content: vec![crate::ai::types::BlockContent::Text(
                                        TextContent {
                                            text: "done".to_string(),
                                            ..Default::default()
                                        },
                                    )],
                                    is_error: false,
                                    timestamp: 1,
                                    ..Default::default()
                                },
                            )),
                            terminate: Some(true),
                            parent_id: None,
                            seq: 0,
                            timestamp: 0,
                        },
                        "main",
                    )
                    .await
                    .expect("tool result");

                assert_eq!(entry.message_terminate(), Some(true));
                let stored = session.get_entry(entry.id()).await.expect("stored");
                assert_eq!(stored.message_terminate(), Some(true));
                assert_eq!(
                    session
                        .find_entries(EntryQuery::default())
                        .await
                        .expect("find"),
                    vec![entry.clone()]
                );
                assert_eq!(
                    session.get_log(LogOptions::default()).await.expect("log"),
                    vec![LogItem::Entry {
                        seq: entry.seq(),
                        entry: entry.clone(),
                    }]
                );
            })
        },
    );

    // Not ported: "validation and immutability :: rejects non-JSON entries
    // before storage mutation". The TypeScript case feeds
    // `appendCustomEntry` loosely typed values (`undefined`, `BigInt`,
    // `NaN`, `Map`, a circular object) that `assertJsonSerializable`
    // rejects. The Rust `Entry` payloads are strongly typed over JSON
    // values, so none of those inputs can be constructed.

    // Not ported: "validation and immutability :: rejects non-JSON records
    // before storage mutation". Same language difference as above: the
    // TypeScript case builds a `tool_started` record whose `effectiveArgs`
    // contain `undefined` or a `BigInt`, which the strongly typed Rust
    // `LaneRecord` cannot represent.

    case(
        &mut cases,
        &fixture,
        "entries and lanes",
        "linearizes concurrent writes across two lanes",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("session").await.expect("create");
                session
                    .append_entry(message_entry("root", create_user_message("root")), "main")
                    .await
                    .expect("root");
                session
                    .create_lane("thread", Some("root".to_string()))
                    .await
                    .expect("lane");
                let completion_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let writes = [
                    ("main-1", "main"),
                    ("thread-1", "thread"),
                    ("main-2", "main"),
                    ("thread-2", "thread"),
                ]
                .into_iter()
                .map(|(id, lane)| {
                    let session = session.clone();
                    let completion_order = Arc::clone(&completion_order);
                    async move {
                        let entry = session
                            .append_entry(custom_entry(id, "note", None), lane)
                            .await
                            .expect("concurrent append");
                        completion_order
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(entry.id().to_string());
                        entry
                    }
                });
                let entries = futures::future::join_all(writes).await;
                let mut sorted = entries.clone();
                sorted.sort_by_key(|entry| entry.seq());
                let commit_order: Vec<String> =
                    sorted.iter().map(|entry| entry.id().to_string()).collect();

                let unique_seqs: std::collections::HashSet<u64> =
                    entries.iter().map(|entry| entry.seq()).collect();
                assert_eq!(unique_seqs.len(), entries.len());
                assert_eq!(*completion_order.lock().unwrap(), commit_order);
                let concurrent_ids: std::collections::HashSet<&str> =
                    entries.iter().map(|entry| entry.id()).collect();
                let log = session.get_log(LogOptions::default()).await.expect("log");
                let log_entry_ids: Vec<String> = log
                    .iter()
                    .filter_map(|item| match item {
                        LogItem::Entry { entry, .. } if concurrent_ids.contains(entry.id()) => {
                            Some(entry.id().to_string())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(log_entry_ids, commit_order);
                let sequences = log_seqs(&log);
                let mut sorted_sequences = sequences.clone();
                sorted_sequences.sort_unstable();
                assert_eq!(sequences, sorted_sequences);
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "creates lists and opens sessions",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("one").await.expect("create");
                let entry_id = session
                    .append_message(create_user_message("persisted"))
                    .await
                    .expect("append");
                let metadata = session.get_metadata().await;

                let listed = fixture.list().await;
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id(), metadata.id);
                assert_eq!(listed[0].created_at(), metadata.created_at);
                assert_eq!(
                    listed[0].parent_session_id(),
                    metadata.parent_session_id.as_deref()
                );
                let repo_metadata = fixture.session_metadata(&session).await;
                let opened = fixture.open(&repo_metadata).await.expect("open");
                assert_eq!(
                    entry_ids(
                        &opened
                            .find_entries(EntryQuery::default())
                            .await
                            .expect("find")
                    ),
                    vec![entry_id]
                );
                rejects_with_code(fixture.create("one").await, SessionErrorCode::AlreadyExists);
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "deletes sessions idempotently",
        |fixture| {
            Box::pin(async move {
                let session = fixture.create("one").await.expect("create");
                let metadata = fixture.session_metadata(&session).await;

                fixture.delete(&metadata).await.expect("delete");
                rejects_with_code(fixture.open(&metadata).await, SessionErrorCode::NotFound);
                fixture.delete(&metadata).await.expect("idempotent delete");
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "forks one branch with selected facts and no records",
        |fixture| {
            Box::pin(async move {
                let source = fixture.create("source").await.expect("create");
                let root = source
                    .append_message(create_user_message("root"))
                    .await
                    .expect("root");
                let shared = source
                    .append_message(create_assistant_message("shared"))
                    .await
                    .expect("shared");
                source
                    .create_lane("thread", Some(shared.clone()))
                    .await
                    .expect("lane");
                let thread_child = source
                    .view("thread")
                    .append_message(create_user_message("thread"))
                    .await
                    .expect("thread child");
                let main_child = source
                    .append_message(create_user_message("main"))
                    .await
                    .expect("main child");
                source
                    .set_name(Some("Source".to_string()))
                    .await
                    .expect("name");
                source
                    .set_label(&shared, Some("copied".to_string()))
                    .await
                    .expect("shared label");
                source
                    .set_label(&thread_child, Some("excluded".to_string()))
                    .await
                    .expect("thread label");
                source
                    .append_record(operation_started("run", "main", "run"))
                    .await
                    .expect("record");
                source
                    .append_record(usage_record(
                        "source-usage",
                        "main",
                        Usage {
                            input: 10,
                            output: 5,
                            cache_read: 3,
                            cache_write: 2,
                            total_tokens: 20,
                            cost: UsageCost {
                                input: 1.0.into(),
                                output: 2.0.into(),
                                cache_read: 3.0.into(),
                                cache_write: 4.0.into(),
                                total: 10.0.into(),
                            },
                            ..Default::default()
                        },
                        UsageCause::Adjustment {
                            run_id: None,
                            entry_id: None,
                            details: None,
                        },
                    ))
                    .await
                    .expect("usage");

                let source_metadata = fixture.session_metadata(&source).await;
                let fork = fixture
                    .fork(
                        &source_metadata,
                        ForkOptions::Branch {
                            entry_id: Some(main_child.clone()),
                            position: Some(ForkPosition::At),
                        },
                        "branch-fork",
                    )
                    .await
                    .expect("fork");

                assert_eq!(
                    entry_ids(
                        &fork
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("find")
                    ),
                    ids(&[&root, &shared, &main_child])
                );
                assert_eq!(
                    fork.get_lanes().await,
                    vec![LanePointer {
                        lane: "main".to_string(),
                        leaf_id: Some(main_child.clone()),
                    }]
                );
                assert_eq!(fork.get_name().await.as_deref(), Some("Source"));
                assert_eq!(fork.get_label(&shared).await.as_deref(), Some("copied"));
                assert_eq!(fork.get_label(&thread_child).await, None);
                assert!(
                    fork.find_records(RecordQuery::default())
                        .await
                        .expect("records")
                        .is_empty()
                );
                assert_eq!(
                    fork.get_stats().await,
                    SessionStats {
                        message_count: 3,
                        cached_tokens: 0,
                        uncached_tokens: 0,
                        total_tokens: 0,
                        cost_total: 0.0,
                    }
                );
                fork.append_message(create_user_message("after fork"))
                    .await
                    .expect("append");
                assert_eq!(fork.get_stats().await.message_count, 4);
                let fork_metadata = fork.get_metadata().await;
                assert_eq!(
                    (
                        fork_metadata.id.as_str(),
                        fork_metadata.parent_session_id.as_deref()
                    ),
                    ("branch-fork", Some("source"))
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "forks a complete tree with lanes and facts",
        |fixture| {
            Box::pin(async move {
                let source = fixture.create("source").await.expect("create");
                let root = source
                    .append_message(create_user_message("root"))
                    .await
                    .expect("root");
                source
                    .create_lane("thread", Some(root.clone()))
                    .await
                    .expect("lane");
                let main_child = source
                    .append_message(create_user_message("main"))
                    .await
                    .expect("main child");
                let thread_child = source
                    .view("thread")
                    .append_message(create_user_message("thread"))
                    .await
                    .expect("thread child");
                source
                    .set_label(&thread_child, Some("thread-tip".to_string()))
                    .await
                    .expect("label");

                let source_metadata = fixture.session_metadata(&source).await;
                let fork = fixture
                    .fork(&source_metadata, ForkOptions::Tree, "tree-fork")
                    .await
                    .expect("fork");
                assert_eq!(
                    entry_ids(
                        &fork
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("find")
                    ),
                    ids(&[&root, &main_child, &thread_child])
                );
                assert_eq!(
                    fork.get_lanes().await,
                    vec![
                        LanePointer {
                            lane: "main".to_string(),
                            leaf_id: Some(main_child.clone()),
                        },
                        LanePointer {
                            lane: "thread".to_string(),
                            leaf_id: Some(thread_child.clone()),
                        },
                    ]
                );
                assert_eq!(
                    fork.get_label(&thread_child).await.as_deref(),
                    Some("thread-tip")
                );
                assert_eq!(fork.get_stats().await.message_count, 3);
                let lane_items: Vec<LogItem> = fork
                    .get_log(LogOptions::default())
                    .await
                    .expect("log")
                    .into_iter()
                    .filter(|item| matches!(item, LogItem::Lane { .. }))
                    .collect();
                assert_eq!(
                    lane_items,
                    vec![
                        LogItem::Lane {
                            seq: 4,
                            lane: "main".to_string(),
                            leaf_id: Some(main_child.clone()),
                        },
                        LogItem::Lane {
                            seq: 5,
                            lane: "thread".to_string(),
                            leaf_id: Some(thread_child.clone()),
                        },
                    ]
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "forks before an entry without modifying the source",
        |fixture| {
            Box::pin(async move {
                let source = fixture.create("source").await.expect("create");
                let root = source
                    .append_message(create_user_message("root"))
                    .await
                    .expect("root");
                let tail = source
                    .append_message(create_user_message("tail"))
                    .await
                    .expect("tail");
                let source_metadata = fixture.session_metadata(&source).await;
                let fork = fixture
                    .fork(
                        &source_metadata,
                        ForkOptions::Branch {
                            entry_id: Some(tail.clone()),
                            position: None,
                        },
                        "fork",
                    )
                    .await
                    .expect("fork");

                assert_eq!(
                    entry_ids(
                        &fork
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("find")
                    ),
                    ids(&[&root])
                );
                assert_eq!(
                    fork.get_leaf_id().await.expect("fork leaf"),
                    Some(root.clone())
                );
                assert_eq!(
                    source.get_leaf_id().await.expect("source leaf"),
                    Some(tail.clone())
                );
                let before_default_target = fixture
                    .fork(
                        &source_metadata,
                        ForkOptions::Branch {
                            entry_id: None,
                            position: Some(ForkPosition::Before),
                        },
                        "before-default-target",
                    )
                    .await
                    .expect("fork");
                assert_eq!(
                    entry_ids(
                        &before_default_target
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("find")
                    ),
                    ids(&[&root])
                );
                assert_eq!(
                    before_default_target.get_leaf_id().await.expect("leaf"),
                    Some(root.clone())
                );

                let at_default_target = fixture
                    .fork(
                        &source_metadata,
                        ForkOptions::Branch {
                            entry_id: None,
                            position: Some(ForkPosition::At),
                        },
                        "at-default-target",
                    )
                    .await
                    .expect("fork");
                assert_eq!(
                    entry_ids(
                        &at_default_target
                            .find_entries(EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            })
                            .await
                            .expect("find")
                    ),
                    ids(&[&root, &tail])
                );
                assert_eq!(
                    at_default_target.get_leaf_id().await.expect("leaf"),
                    Some(tail.clone())
                );
                rejects_with_code(
                    fixture
                        .fork(
                            &source_metadata,
                            ForkOptions::Branch {
                                entry_id: Some("missing".to_string()),
                                position: None,
                            },
                            "missing-fork",
                        )
                        .await,
                    SessionErrorCode::InvalidForkTarget,
                );
            })
        },
    );

    case(
        &mut cases,
        &fixture,
        "repository and forks",
        "validates the default fork target",
        |fixture| {
            Box::pin(async move {
                let source = fixture
                    .create("source-with-custom-leaf")
                    .await
                    .expect("create");
                source
                    .append_custom_entry("not-a-message", None)
                    .await
                    .expect("custom entry");

                let metadata = fixture.session_metadata(&source).await;
                rejects_with_code(
                    fixture
                        .fork(
                            &metadata,
                            ForkOptions::Branch {
                                entry_id: None,
                                position: None,
                            },
                            "fork",
                        )
                        .await,
                    SessionErrorCode::InvalidForkTarget,
                );
            })
        },
    );

    cases
}

/// Runs every conformance case against the in-memory backend, asserting
/// each case completes without panicking.
pub async fn run_in_memory_conformance() {
    let fixture = Arc::new(SessionBackendFixture::InMemory(InMemorySessionRepo::new()));
    for case in create_session_backend_conformance(fixture) {
        let joined = tokio::spawn((case.run)()).await;
        assert!(
            joined.is_ok(),
            "conformance case failed: {}/{}",
            case.group,
            case.name
        );
    }
}

/// Builds a JSONL fixture rooted in a fresh directory under `root`.
pub fn jsonl_fixture(env: &Arc<NodeExecutionEnv>, root: &str) -> SessionBackendFixture {
    SessionBackendFixture::Jsonl(JsonlSessionFixture {
        repo: JsonlSessionRepo::new(JsonlSessionRepoOptions {
            fs: Arc::clone(env) as Arc<dyn FileSystem>,
            sessions_root: format!("{root}/sessions"),
        }),
        fs: Arc::clone(env) as Arc<dyn FileSystem>,
        sessions_base: root.to_string(),
    })
}

/// Storage-trait accessor used by JSONL conformance wiring.
pub fn storage_of(
    session: &Session,
) -> Arc<dyn crate::agent::harness::session::types::SessionStorage> {
    Arc::clone(session.storage())
}
