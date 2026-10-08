//! Golden and behavior tests for the JSONL v4 session port
//! (`jsonl-codec.test.ts`, the durable-persistence cases from
//! `jsonl.test.ts`, and byte parity against the TypeScript oracle goldens
//! produced by `scripts/oracle/export-session-jsonl.mts`).

mod common;

use std::sync::Arc;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::session::jsonl::codec::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
};
use pi_core::agent::harness::session::jsonl::errors::JsonlDecodeErrorKind;
use pi_core::agent::harness::session::jsonl::repo::{JsonlSessionRepo, load_jsonl_session_storage};
use pi_core::agent::harness::session::jsonl::storage::JsonlSessionStorage;
use pi_core::agent::harness::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlV4Header,
};
use pi_core::agent::harness::session::memory::{Session, set_clock_override};
use pi_core::agent::harness::session::state::SessionMutation;
use pi_core::agent::harness::session::types::{
    Entry, ForkOptions, LaneRecord, OperationIntent, SessionErrorCode, UsageCause,
};
use pi_core::agent::harness::types::{FileSystem, WriteContent};
use pi_core::agent::types::AgentMessage;
use pi_core::ai::types::{
    AssistantContent, BlockContent, RoleAssistant, RoleUser, StopReason, TextContent, Usage,
    UserContent, UserMessage,
};

const FIXED_NOW: i64 = 1_700_000_000_000;

/// The clock override is process-global; serialize the tests that use it.
fn clock_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ClockGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl ClockGuard {
    fn install() -> Self {
        let guard = clock_lock();
        set_clock_override(Some(Arc::new(|| FIXED_NOW)));
        ClockGuard(guard)
    }
}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        set_clock_override(None);
    }
}

fn golden(name: &str) -> String {
    let path = format!(
        "{}/tests/goldens/session-jsonl/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("missing golden {name}: {error}"))
}

fn usage_fixture() -> Usage {
    Usage {
        input: 10,
        output: 20,
        cache_read: 5,
        cache_write: 0,
        total_tokens: 35,
        cost: pi_core::ai::types::UsageCost {
            input: 0.1.into(),
            output: 0.2.into(),
            cache_read: 0.05.into(),
            cache_write: 0.0.into(),
            total: 0.35.into(),
        },
        ..Default::default()
    }
}

fn user_text_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })]),
        timestamp: FIXED_NOW,
    })
}

fn assistant_tool_use_message() -> AgentMessage {
    AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![
            AssistantContent::Thinking(pi_core::ai::types::ThinkingContent {
                thinking: "hmm".to_string(),
                thinking_signature: Some("sig".to_string()),
                ..Default::default()
            }),
            AssistantContent::Text(TextContent {
                text: "hi there".to_string(),
                ..Default::default()
            }),
            AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "ls" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ..Default::default()
            }),
        ],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage: usage_fixture(),
        stop_reason: StopReason::ToolUse,
        timestamp: FIXED_NOW,
        ..Default::default()
    }))
}

fn header(id: &str) -> JsonlV4Header {
    JsonlV4Header {
        version: 4,
        id: id.to_string(),
        created_at: FIXED_NOW,
        cwd: "/workspace/project".to_string(),
        parent_session_id: None,
        legacy_parent_session_path: None,
        metadata: None,
    }
}

/// Replays the oracle scenario through the Rust storage and API.
async fn replay_oracle_scenario(env: &Arc<NodeExecutionEnv>, root: &str) -> String {
    let path = format!("{root}/session.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::clone(env) as Arc<dyn FileSystem>,
        &path,
        header("golden-session"),
    )
    .await
    .unwrap();
    let next_entry_id = std::sync::atomic::AtomicU32::new(0);
    let session = Session::with_id_generator(
        storage,
        Arc::new(move || {
            format!(
                "entry-{}",
                next_entry_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            )
        }),
    );

    session
        .append_message(user_text_message("hello"))
        .await
        .unwrap();
    session
        .append_message(assistant_tool_use_message())
        .await
        .unwrap();
    session
        .append_message(AgentMessage::ToolResult(Box::new(
            pi_core::ai::types::ToolResultMessage {
                role: Default::default(),
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                content: vec![BlockContent::Text(TextContent {
                    text: "files".to_string(),
                    ..Default::default()
                })],
                details: Some(serde_json::json!({ "exitCode": 0 })),
                is_error: false,
                timestamp: FIXED_NOW,
                ..Default::default()
            },
        )))
        .await
        .unwrap();
    session
        .append_custom_entry("note", Some(serde_json::json!({ "text": "a custom note" })))
        .await
        .unwrap();
    session
        .append_entry(
            Entry::ModelChange {
                id: "model-1".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                provider: "openai".to_string(),
                model_id: "gpt-5".to_string(),
            },
            "main",
        )
        .await
        .unwrap();
    session
        .append_entry(
            Entry::ThinkingLevelChange {
                id: "thinking-1".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                thinking_level: "high".to_string(),
            },
            "main",
        )
        .await
        .unwrap();
    session
        .append_entry(
            Entry::ActiveToolsChange {
                id: "tools-1".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                active_tool_names: vec!["bash".to_string(), "read".to_string()],
            },
            "main",
        )
        .await
        .unwrap();
    session
        .append_record(LaneRecord::OperationStarted {
            id: "run-1".to_string(),
            lane: "main".to_string(),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
            seq: 0,
            timestamp: 0,
        })
        .await
        .unwrap();
    session
        .append_record(LaneRecord::Usage {
            id: "usage-1".to_string(),
            lane: "main".to_string(),
            usage: usage_fixture(),
            cause: UsageCause::Assistant {
                run_id: "run-1".to_string(),
                entry_id: "entry-2".to_string(),
                attempt: 1,
                stop_reason: "toolUse".to_string(),
            },
            seq: 0,
            timestamp: 0,
        })
        .await
        .unwrap();
    session
        .append_record(LaneRecord::OperationFinished {
            id: "finish-1".to_string(),
            lane: "main".to_string(),
            run_id: "run-1".to_string(),
            outcome: "completed".to_string(),
            error: None,
            seq: 0,
            timestamp: 0,
        })
        .await
        .unwrap();
    session
        .create_lane("thread", Some("entry-1".to_string()))
        .await
        .unwrap();
    session
        .append_entry(
            Entry::Custom {
                id: "thread-note".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                custom_type: "thread".to_string(),
                data: None,
            },
            "thread",
        )
        .await
        .unwrap();
    session
        .set_name(Some("Golden session".to_string()))
        .await
        .unwrap();
    session
        .set_label("entry-1", Some("checkpoint".to_string()))
        .await
        .unwrap();

    env.read_text_file(&path, None).await.unwrap()
}

#[tokio::test]
async fn rust_session_bytes_match_the_typescript_oracle() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env: Arc<NodeExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }));

    let written = replay_oracle_scenario(&env, &root).await;
    let expected = golden("session.jsonl");
    assert_eq!(
        written, expected,
        "session JSONL bytes must match the oracle"
    );
}

#[tokio::test]
async fn oracle_golden_lines_round_trip_through_the_rust_codec() {
    for name in [
        "header-line.jsonl",
        "header-legacy.jsonl",
        "mutation-entry-message.jsonl",
        "mutation-entry-custom.jsonl",
        "mutation-record-usage.jsonl",
        "mutation-lane.jsonl",
        "mutation-fact-name.jsonl",
        "mutation-fact-label.jsonl",
        "mutation-fact-cleared.jsonl",
    ] {
        let golden_line = golden(name);
        let line = golden_line.trim_end();
        if name.starts_with("header") {
            let header = parse_header(line).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(encode_header(&header), golden_line, "{name} re-encode");
        } else {
            let mutation = parse_mutation(line).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(encode_mutation(&mutation), golden_line, "{name} re-encode");
        }
    }
}

#[tokio::test]
async fn compaction_and_branch_summary_goldens_round_trip() {
    for name in ["compaction.jsonl", "branch-summary.jsonl"] {
        let content = golden(name);
        let mut lines = content.split('\n').collect::<Vec<&str>>();
        lines.pop();
        parse_header(lines[0]).unwrap_or_else(|error| panic!("{name} header: {error:?}"));
        for line in &lines[1..] {
            parse_mutation(line).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        }
    }
}

#[tokio::test]
async fn codec_returns_syntax_and_schema_errors() {
    let error = parse_mutation("{").unwrap_err();
    assert_eq!(error.kind, JsonlDecodeErrorKind::Syntax);
    let error = parse_mutation(&serde_json::json!({ "kind": "unknown", "seq": 1 }).to_string())
        .unwrap_err();
    assert_eq!(error.kind, JsonlDecodeErrorKind::Schema);
    assert_eq!(error.message, "has unknown mutation kind");
}

#[tokio::test]
async fn codec_rejects_incomplete_mutations() {
    let cases = [
        serde_json::json!({ "kind": "entry", "type": "custom", "id": "entry", "parentId": null, "seq": 1, "timestamp": 1 }),
        serde_json::json!({ "kind": "record", "type": "operation_started", "id": "run", "lane": "main", "seq": 1, "timestamp": 1, "sourceLeafId": null }),
        serde_json::json!({ "kind": "record", "type": "operation_finished", "id": "finish", "lane": "main", "seq": 1, "timestamp": 1, "outcome": "completed" }),
    ];
    for case in cases {
        let error = parse_mutation(&case.to_string())
            .unwrap_err_or_expected(format!("expected rejection for {case}"));
        assert_eq!(error.kind, JsonlDecodeErrorKind::Schema, "{case}");
    }
}

#[tokio::test]
async fn metadata_from_header_projects_all_fields() {
    let header = JsonlV4Header {
        version: 4,
        id: "session".to_string(),
        created_at: 1_700_000_000_000,
        cwd: "/workspace/project".to_string(),
        parent_session_id: None,
        legacy_parent_session_path: Some("/sessions/missing-parent.jsonl".to_string()),
        metadata: Some(serde_json::json!({ "owner": "agent" })),
    };
    let metadata = metadata_from_header(&header, "/sessions/session.jsonl", 1_700_000_000_100.0);
    assert_eq!(metadata.id, "session");
    assert_eq!(metadata.created_at, 1_700_000_000_000);
    assert_eq!(metadata.cwd, "/workspace/project");
    assert_eq!(metadata.path, "/sessions/session.jsonl");
    assert_eq!(metadata.modified_at, 1_700_000_000_100.0);
    assert_eq!(metadata.source_format, 4);
    assert_eq!(
        metadata.legacy_parent_session_path.as_deref(),
        Some("/sessions/missing-parent.jsonl")
    );
    assert_eq!(
        metadata.metadata,
        Some(serde_json::json!({ "owner": "agent" }))
    );
}

fn env_for(root: &str) -> Arc<NodeExecutionEnv> {
    Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }))
}

fn repo_for(env: &Arc<NodeExecutionEnv>, root: &str) -> JsonlSessionRepo {
    JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::clone(env) as Arc<dyn FileSystem>,
        sessions_root: format!("{root}/sessions"),
    })
}

#[tokio::test]
async fn repo_exposes_the_complete_metadata_contract() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);

    let session = repo
        .create(JsonlSessionCreateOptions {
            id: Some("session".to_string()),
            parent_session_id: Some("parent".to_string()),
            cwd: root.to_string(),
            metadata: Some(serde_json::json!({ "owner": "agent" })),
        })
        .await
        .unwrap();
    session
        .append_message(user_text_message("hello"))
        .await
        .unwrap();

    let listed = repo
        .list(&JsonlSessionListOptions::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "session");
    assert_eq!(listed[0].cwd, root.to_string());
    assert_eq!(listed[0].parent_session_id.as_deref(), Some("parent"));
    assert_eq!(
        listed[0].metadata,
        Some(serde_json::json!({ "owner": "agent" }))
    );
    assert_eq!(listed[0].source_format, 4);
    assert!(listed[0].path.ends_with("session.jsonl"));

    let reopened = repo.open(listed[0].clone()).await.unwrap();
    assert_eq!(reopened.get_metadata().await.id, "session");
    assert_eq!(reopened.get_stats().await.message_count, 1);
}

#[tokio::test]
async fn repo_rejects_malformed_headers_on_open_and_skips_them_when_listing() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);
    let session = repo
        .create(JsonlSessionCreateOptions {
            id: Some("good".to_string()),
            cwd: root.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = session;

    let listed = repo
        .list(&JsonlSessionListOptions::default())
        .await
        .unwrap();
    let bad_path = format!("{}.bad.jsonl", listed[0].path.trim_end_matches(".jsonl"));
    env.write_file(
        &bad_path,
        &WriteContent::Text("{\"kind\":\"header\"}\n".to_string()),
        None,
    )
    .await
    .unwrap();
    env.write_file(
        &format!("{bad_path}.meta.jsonl"),
        &WriteContent::Text(
            "{\"kind\":\"header\",\"version\":4,\"id\":\"meta\",\"createdAt\":1,\"cwd\":\"/w\",\"metadata\":5}\n"
                .to_string(),
        ),
        None,
    )
    .await
    .unwrap();

    let listed = repo
        .list(&JsonlSessionListOptions::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "good");

    for bad in [bad_path.clone(), format!("{bad_path}.meta.jsonl")] {
        let metadata = JsonlSessionMetadata {
            id: "x".to_string(),
            created_at: 0,
            cwd: String::new(),
            path: bad.clone(),
            modified_at: 0.0,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };
        let result = load_jsonl_session_storage(
            &JsonlSessionRepoOptions {
                fs: Arc::clone(&env) as Arc<dyn FileSystem>,
                sessions_root: format!("{root}/sessions"),
            },
            &metadata,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected rejection for {bad}"),
        };
        assert!(
            error.message.contains("Invalid JSONL v4 session"),
            "unexpected error: {error}"
        );
    }
}

#[tokio::test]
async fn repo_rejects_session_ids_that_cannot_be_used_in_filenames() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);
    for bad_id in ["-leading", "trailing-", "with space", "a/b"] {
        let result = repo
            .create(JsonlSessionCreateOptions {
                id: Some(bad_id.to_string()),
                cwd: root.to_string(),
                ..Default::default()
            })
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected rejection for {bad_id}"),
        };
        assert_eq!(error.code, SessionErrorCode::InvalidPayload, "{bad_id}");
    }
}

#[tokio::test]
async fn repo_allows_the_same_explicit_session_id_in_different_directories() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);
    let one = repo
        .create(JsonlSessionCreateOptions {
            id: Some("shared".to_string()),
            cwd: format!("{root}/one"),
            ..Default::default()
        })
        .await
        .unwrap();
    let two = repo
        .create(JsonlSessionCreateOptions {
            id: Some("shared".to_string()),
            cwd: format!("{root}/two"),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = (one, two);
    assert_eq!(
        repo.list(&JsonlSessionListOptions::default())
            .await
            .unwrap()
            .len(),
        2
    );

    let result = repo
        .create(JsonlSessionCreateOptions {
            id: Some("shared".to_string()),
            cwd: format!("{root}/one"),
            ..Default::default()
        })
        .await;
    let duplicate = match result {
        Err(error) => error,
        Ok(_) => panic!("expected duplicate rejection"),
    };
    assert_eq!(duplicate.code, SessionErrorCode::AlreadyExists);
}

#[tokio::test]
async fn repo_writes_one_line_per_mutation_and_restores_the_sequence() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);
    let session = repo
        .create(JsonlSessionCreateOptions {
            id: Some("lines".to_string()),
            cwd: root.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    session
        .append_message(user_text_message("one"))
        .await
        .unwrap();
    session.set_name(Some("Named".to_string())).await.unwrap();
    session
        .append_message(user_text_message("two"))
        .await
        .unwrap();

    let listed = repo
        .list(&JsonlSessionListOptions::default())
        .await
        .unwrap();
    let content = env.read_text_file(&listed[0].path, None).await.unwrap();
    let lines: Vec<&str> = content.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 4); // header + 3 mutations
    for (index, line) in lines.iter().enumerate().skip(1) {
        let mutation = parse_mutation(line).unwrap();
        assert_eq!(mutation_seq(&mutation), index as u64);
    }

    let reopened = repo.open(listed[0].clone()).await.unwrap();
    assert_eq!(reopened.get_stats().await.message_count, 2);
    reopened
        .append_message(user_text_message("three"))
        .await
        .unwrap();
    let content = env.read_text_file(&listed[0].path, None).await.unwrap();
    let last = content.trim_end().split('\n').next_back().unwrap();
    assert_eq!(mutation_seq(&parse_mutation(last).unwrap()), 4);
}

fn mutation_seq(mutation: &SessionMutation) -> u64 {
    match mutation {
        SessionMutation::Entry { entry, .. } => entry.seq(),
        SessionMutation::Record { record } => record.seq(),
        SessionMutation::Lane { seq, .. }
        | SessionMutation::NameFact { seq, .. }
        | SessionMutation::LabelFact { seq, .. } => *seq,
    }
}

#[tokio::test]
async fn repo_forks_recompute_message_counts_and_carry_facts() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let repo = repo_for(&env, &root);
    let session = repo
        .create(JsonlSessionCreateOptions {
            id: Some("source".to_string()),
            cwd: root.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    session
        .append_message(user_text_message("one"))
        .await
        .unwrap();
    session
        .append_message(user_text_message("two"))
        .await
        .unwrap();
    session.set_name(Some("Fork me".to_string())).await.unwrap();

    let source_metadata = repo
        .list(&JsonlSessionListOptions::default())
        .await
        .unwrap()
        .remove(0);
    let forked = repo
        .fork(
            source_metadata,
            ForkOptions::Tree,
            JsonlSessionCreateOptions {
                id: Some("forked".to_string()),
                cwd: root.to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        forked.get_metadata().await.parent_session_id.as_deref(),
        Some("source")
    );
    assert_eq!(forked.get_stats().await.message_count, 2);
    assert_eq!(forked.get_name().await.as_deref(), Some("Fork me"));
}

#[tokio::test]
async fn storage_repairs_a_valid_final_line_missing_its_newline() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let path = format!("{root}/unterminated.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::clone(&env) as Arc<dyn FileSystem>,
        &path,
        header("unterminated"),
    )
    .await
    .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user_text_message("hello"))
        .await
        .unwrap();

    let content = env.read_text_file(&path, None).await.unwrap();
    env.write_file(
        &path,
        &WriteContent::Text(content.trim_end().to_string()),
        None,
    )
    .await
    .unwrap();

    let reloaded = JsonlSessionStorage::load(Arc::clone(&env) as Arc<dyn FileSystem>, &path)
        .await
        .unwrap();
    let session = Session::new(reloaded);
    assert_eq!(session.get_stats().await.message_count, 1);
    let repaired = env.read_text_file(&path, None).await.unwrap();
    assert!(repaired.ends_with('\n'));
}

#[tokio::test]
async fn storage_truncates_a_malformed_final_line() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let path = format!("{root}/torn.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::clone(&env) as Arc<dyn FileSystem>,
        &path,
        header("torn"),
    )
    .await
    .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user_text_message("hello"))
        .await
        .unwrap();

    let content = env.read_text_file(&path, None).await.unwrap();
    env.write_file(
        &path,
        &WriteContent::Text(format!(
            "{content}{{\"kind\":\"entry\",\"seq\":2,\"type\":\"mes"
        )),
        None,
    )
    .await
    .unwrap();

    let reloaded = JsonlSessionStorage::load(Arc::clone(&env) as Arc<dyn FileSystem>, &path)
        .await
        .unwrap();
    let session = Session::new(reloaded);
    assert_eq!(session.get_stats().await.message_count, 1);
    let repaired = env.read_text_file(&path, None).await.unwrap();
    assert_eq!(repaired, content);
    assert!(!repaired.contains("\"seq\":2"));
}

#[tokio::test]
async fn storage_rejects_a_complete_invalid_final_mutation_without_modifying_the_file() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let path = format!("{root}/bad-final.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::clone(&env) as Arc<dyn FileSystem>,
        &path,
        header("bad-final"),
    )
    .await
    .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user_text_message("hello"))
        .await
        .unwrap();

    env.append_file(
        &path,
        &WriteContent::Text("{\"kind\":\"fact\",\"seq\":2,\"fact\":\"bogus\"}\n".to_string()),
        None,
    )
    .await
    .unwrap();
    let before = env.read_text_file(&path, None).await.unwrap();

    let result = JsonlSessionStorage::load(Arc::clone(&env) as Arc<dyn FileSystem>, &path).await;
    match result {
        Err(error) => assert_eq!(error.code, SessionErrorCode::InvalidEntry),
        Ok(_) => panic!("expected rejection"),
    }
    assert_eq!(env.read_text_file(&path, None).await.unwrap(), before);
}

#[tokio::test]
async fn storage_rejects_a_malformed_middle_line_without_modifying_the_file() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let path = format!("{root}/bad-middle.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::clone(&env) as Arc<dyn FileSystem>,
        &path,
        header("bad-middle"),
    )
    .await
    .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user_text_message("one"))
        .await
        .unwrap();
    session
        .append_message(user_text_message("two"))
        .await
        .unwrap();
    let content = env.read_text_file(&path, None).await.unwrap();
    let mut lines: Vec<&str> = content.trim_end().split('\n').collect();
    lines.insert(2, "{\"no\":\"parse\"");
    env.write_file(
        &path,
        &WriteContent::Text(format!("{}\n", lines.join("\n"))),
        None,
    )
    .await
    .unwrap();

    let result = JsonlSessionStorage::load(Arc::clone(&env) as Arc<dyn FileSystem>, &path).await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("expected rejection"),
    };
    assert_eq!(error.code, SessionErrorCode::InvalidEntry);
    assert!(
        error.message.contains("line 3"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn storage_rejects_entries_referencing_missing_parents() {
    let _clock = ClockGuard::install();
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let path = format!("{root}/orphan.jsonl");
    let header_line = encode_header(&header("orphan"));
    let orphan = format!(
        "{}{}\n",
        header_line.trim_end(),
        serde_json::json!({
            "kind": "entry",
            "type": "custom",
            "id": "orphan",
            "parentId": "missing",
            "seq": 1,
            "timestamp": 1,
            "customType": "note"
        })
    );
    env.write_file(&path, &WriteContent::Text(orphan), None)
        .await
        .unwrap();
    let result = JsonlSessionStorage::load(Arc::clone(&env) as Arc<dyn FileSystem>, &path).await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("expected rejection"),
    };
    assert_eq!(error.code, SessionErrorCode::InvalidEntry);
}

#[tokio::test]
async fn session_file_name_matches_the_typescript_timestamp_format() {
    // Verified against `new Date(1700000000000).toISOString()` in the
    // oracle environment.
    use pi_core::agent::harness::session::jsonl::repo::session_file_name;
    assert_eq!(
        session_file_name(1_700_000_000_000, "id"),
        "2023-11-14T22-13-20-000Z_id.jsonl"
    );
}

trait ExpectedErrExt<E: std::fmt::Debug> {
    fn unwrap_err_or_expected(self, message: String) -> E;
}

impl<T, E: std::fmt::Debug> ExpectedErrExt<E> for Result<T, E> {
    fn unwrap_err_or_expected(self, message: String) -> E {
        match self {
            Err(error) => error,
            Ok(_) => panic!("expected rejection: {message}"),
        }
    }
}
