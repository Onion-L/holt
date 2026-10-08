//! Port of `pi-core/agent/test/harness/session/context.test.ts` and
//! `memory.test.ts` (the conformance suite itself lands with the JSONL
//! backend port; the id-generator lane-view case is ported here).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pi_core::agent::harness::session::context::{
    SessionContextBuildOptions, build_session_context,
};
use pi_core::agent::harness::session::memory::{
    InMemorySessionRepo, InMemorySessionStorage, Session,
};
use pi_core::agent::harness::session::types::{Entry, EntryType, SessionMetadata};
use pi_core::agent::types::AgentMessage;
use pi_core::ai::types::{
    AssistantContent, BlockContent, DeferredHandle, RoleAssistant, RoleUser, StopReason,
    TextContent, UserContent, UserMessage,
};

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })]),
        timestamp: 1,
    })
}

fn assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }))
}

fn message_entry(id: &str, parent_id: Option<&str>, message: AgentMessage, seq: u64) -> Entry {
    Entry::Message {
        id: id.to_string(),
        seq,
        parent_id: parent_id.map(str::to_string),
        timestamp: seq as i64,
        message,
        terminate: None,
    }
}

fn roles(messages: &[AgentMessage]) -> Vec<String> {
    messages.iter().map(|m| m.role().to_string()).collect()
}

#[tokio::test]
async fn starts_at_the_latest_compaction_and_materializes_its_retained_tail() {
    let entries = vec![
        message_entry("old", None, user_message("old"), 1),
        Entry::Compaction {
            id: "compact".to_string(),
            seq: 2,
            parent_id: Some("old".to_string()),
            timestamp: 2,
            summary: "summary".to_string(),
            retained_tail: vec![user_message("retained"), assistant_message("answer")],
            tokens_before: 100,
            details: None,
            usage: None,
        },
        Entry::ModelChange {
            id: "model".to_string(),
            seq: 3,
            parent_id: Some("compact".to_string()),
            timestamp: 3,
            provider: "openai".to_string(),
            model_id: "gpt-5".to_string(),
        },
        Entry::ThinkingLevelChange {
            id: "thinking".to_string(),
            seq: 4,
            parent_id: Some("model".to_string()),
            timestamp: 4,
            thinking_level: "high".to_string(),
        },
        message_entry("tail", Some("thinking"), user_message("tail"), 5),
    ];

    let context = build_session_context(&entries, &SessionContextBuildOptions::default());
    assert_eq!(
        roles(&context.messages),
        ["compactionSummary", "user", "assistant", "user"]
    );
    let model = context.model.expect("derived model");
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model_id, "gpt-5");
    assert_eq!(context.thinking_level, "high");
}

#[tokio::test]
async fn applies_caller_transforms_after_the_compaction_boundary() {
    let entries = vec![
        message_entry("old", None, user_message("old"), 1),
        Entry::Compaction {
            id: "compact".to_string(),
            seq: 2,
            parent_id: Some("old".to_string()),
            timestamp: 2,
            summary: "summary".to_string(),
            retained_tail: Vec::new(),
            tokens_before: 100,
            details: None,
            usage: None,
        },
        Entry::BranchSummary {
            id: "branch".to_string(),
            seq: 3,
            parent_id: Some("compact".to_string()),
            timestamp: 3,
            from_id: "abandoned".to_string(),
            summary: "branch summary".to_string(),
            details: None,
            usage: None,
        },
        message_entry("tail", Some("branch"), user_message("tail"), 4),
    ];

    let options = SessionContextBuildOptions {
        entry_transforms: vec![Arc::new(|entries: &[Entry]| {
            entries
                .iter()
                .filter(|entry| entry.entry_type() != EntryType::Compaction)
                .cloned()
                .collect()
        })],
        ..Default::default()
    };
    let context = build_session_context(&entries, &options);
    assert_eq!(roles(&context.messages), ["branchSummary", "user"]);
}

#[tokio::test]
async fn projects_custom_entries_and_omits_deferred_assistant_handles() {
    let mut deferred_assistant = match assistant_message("") {
        AgentMessage::Assistant(assistant) => *assistant,
        _ => unreachable!(),
    };
    deferred_assistant.content = Vec::new();
    deferred_assistant.stop_reason = StopReason::Deferred;
    deferred_assistant.deferred = Some(DeferredHandle {
        provider: "openai".to_string(),
        model_id: "gpt-5".to_string(),
        api: "openai-responses".to_string(),
        id: "response-1".to_string(),
        ..Default::default()
    });
    let entries = vec![
        message_entry("user", None, user_message("hello"), 1),
        message_entry(
            "deferred",
            Some("user"),
            AgentMessage::Assistant(Box::new(deferred_assistant)),
            2,
        ),
        Entry::Custom {
            id: "custom".to_string(),
            seq: 3,
            parent_id: Some("deferred".to_string()),
            timestamp: 3,
            custom_type: "note".to_string(),
            data: Some(serde_json::json!("project me")),
        },
    ];

    let options = SessionContextBuildOptions {
        entry_projectors: [(
            "note".to_string(),
            Arc::new(|entry: &Entry, _index: usize, _entries: &[Entry]| {
                let Entry::Custom { data, .. } = entry else {
                    return None;
                };
                let text = data
                    .as_ref()
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                Some(vec![user_message(&format!("note: {text}"))])
            })
                as pi_core::agent::harness::session::context::CustomEntryContextMessageProjector,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let context = build_session_context(&entries, &options);
    assert_eq!(roles(&context.messages), ["user", "user"]);
    match &context.messages[1] {
        AgentMessage::User(user) => match &user.content {
            UserContent::Blocks(blocks) => match &blocks[0] {
                BlockContent::Text(text) => assert_eq!(text.text, "note: project me"),
                _ => panic!("expected text block"),
            },
            _ => panic!("expected block content"),
        },
        _ => panic!("expected user message"),
    }
}

#[tokio::test]
async fn session_uses_one_injectable_id_generator_across_lane_views() {
    let next_id = AtomicU32::new(0);
    let id_generator =
        Arc::new(move || format!("generated-{}", next_id.fetch_add(1, Ordering::SeqCst) + 1))
            as pi_core::agent::harness::session::memory::IdGenerator;
    let storage = Arc::new(InMemorySessionStorage::new(SessionMetadata {
        id: "session".to_string(),
        created_at: 1,
        parent_session_id: None,
    }));
    let session = Session::with_id_generator(storage, id_generator);
    let main_id = session.append_custom_entry("note", None).await.unwrap();
    session
        .create_lane("thread", Some(main_id.clone()))
        .await
        .unwrap();
    let thread_id = append_custom_on_lane(&session, "thread").await;

    assert_eq!(main_id, "generated-1");
    assert_eq!(thread_id, "generated-2");
}

/// Appends through the raw storage entry API; the lane-view equivalent is
/// `session.view(lane).append_custom_entry(...)`.
async fn append_custom_on_lane(session: &Session, lane: &str) -> String {
    session
        .append_entry(
            Entry::Custom {
                id: (session.id_generator())(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                custom_type: "note".to_string(),
                data: None,
            },
            lane,
        )
        .await
        .expect("lane append")
        .id()
        .to_string()
}

#[tokio::test]
async fn in_memory_repo_create_open_list_delete_fork() {
    let repo = InMemorySessionRepo::new();
    let session = repo
        .create(Default::default())
        .await
        .expect("create session");
    let metadata = session.get_metadata().await;
    assert_eq!(metadata.parent_session_id, None);

    let id = session.append_message(user_message("hello")).await.unwrap();
    session.set_name(Some("named".to_string())).await.unwrap();

    let reopened = repo.open(metadata.clone()).await.expect("open session");
    assert_eq!(reopened.get_name().await.as_deref(), Some("named"));
    assert!(reopened.get_entry(&id).await.is_some());

    let listed = repo.list().await;
    assert_eq!(listed.len(), 1);

    let forked = repo
        .fork(
            metadata.clone(),
            pi_core::agent::harness::session::types::ForkOptions::Tree,
            Default::default(),
        )
        .await
        .expect("fork session");
    assert_eq!(
        forked.get_metadata().await.parent_session_id,
        Some(metadata.id.clone())
    );
    assert_eq!(forked.get_stats().await.message_count, 1);

    repo.delete(metadata).await;
    // The forked session remains listed until it is deleted too.
    let remaining = repo.list().await;
    assert_eq!(remaining.len(), 1);
    repo.delete(remaining.into_iter().next().unwrap()).await;
    assert!(repo.list().await.is_empty());
}
