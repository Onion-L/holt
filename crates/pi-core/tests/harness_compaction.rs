//! Port of `pi-core/agent/test/harness/compaction.test.ts` (the offline
//! preparation/cut-point/token suites; the faux-provider summary suites run
//! through the same Models API in `tests/ai_faux_provider.rs` paths) and
//! `branch-summarization.test.ts`.

mod common;

use std::sync::{Arc, Mutex};

use serde_json::json;

use pi_core::agent::harness::compaction::branch_summarization::{
    GenerateBranchSummaryOptions, collect_entries_for_branch_summary, generate_branch_summary,
};
use pi_core::agent::harness::compaction::compaction::{
    CompactionSettings, CutPointResult, DEFAULT_COMPACTION_SETTINGS, calculate_context_tokens,
    estimate_context_tokens, estimate_tokens, find_cut_point, find_turn_start_index,
    prepare_compaction, should_compact,
};
use pi_core::agent::harness::compaction::utils::serialize_conversation;
use pi_core::agent::harness::session::context::build_session_context;
use pi_core::agent::harness::session::memory::InMemorySessionStorage;
use pi_core::agent::harness::session::memory::Session;
use pi_core::agent::harness::session::types::{Entry, SessionMetadata};
use pi_core::agent::harness::types::CompactionErrorCode;
use pi_core::agent::types::AgentMessage;
use pi_core::ai::models::Models;
use pi_core::ai::providers::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
    faux_provider,
};
use pi_core::ai::types::{
    AssistantContent, BlockContent, RoleAssistant, RoleUser, StopReason, TextContent, Usage,
    UserContent, UserMessage,
};
use pi_core::ai::types::{
    AssistantMessage as AiAssistantMessage, CacheRetention, Context, Message, SimpleStreamOptions,
};

fn mock_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        ..Default::default()
    }
}

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

fn assistant_message(text: &str, usage: Usage) -> AgentMessage {
    AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage,
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }))
}

fn assistant_message_with_content(content: Vec<AssistantContent>, usage: Usage) -> AgentMessage {
    AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage,
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }))
}

fn message_entry(id: u64, message: AgentMessage, parent_id: Option<String>) -> Entry {
    Entry::Message {
        id: format!("entry-{id}"),
        seq: id,
        parent_id,
        timestamp: 1,
        message,
        terminate: None,
    }
}

fn compaction_entry(
    id: u64,
    summary: &str,
    parent_id: Option<String>,
    retained_tail: Vec<AgentMessage>,
) -> Entry {
    Entry::Compaction {
        id: format!("entry-{id}"),
        seq: id,
        parent_id,
        timestamp: 1,
        summary: summary.to_string(),
        retained_tail,
        tokens_before: 1234,
        details: None,
        usage: None,
    }
}

fn thinking_entry(id: u64, level: &str, parent_id: Option<String>) -> Entry {
    Entry::ThinkingLevelChange {
        id: format!("entry-{id}"),
        seq: id,
        parent_id,
        timestamp: 1,
        thinking_level: level.to_string(),
    }
}

fn model_change_entry(id: u64, provider: &str, model_id: &str, parent_id: Option<String>) -> Entry {
    Entry::ModelChange {
        id: format!("entry-{id}"),
        seq: id,
        parent_id,
        timestamp: 1,
        provider: provider.to_string(),
        model_id: model_id.to_string(),
    }
}

#[test]
fn calculates_total_context_tokens_from_usage() {
    assert_eq!(
        calculate_context_tokens(&mock_usage(1000, 500, 200, 100)),
        1800
    );
    assert_eq!(calculate_context_tokens(&mock_usage(0, 0, 0, 0)), 0);
}

#[test]
fn checks_compaction_threshold() {
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10000,
        keep_recent_tokens: 20000,
    };
    assert!(should_compact(95000, 100000, &settings));
    assert!(!should_compact(89000, 100000, &settings));
    assert!(!should_compact(
        95000,
        100000,
        &CompactionSettings {
            enabled: false,
            ..settings
        }
    ));
}

#[test]
fn finds_a_cut_point_based_on_token_differences() {
    let mut entries: Vec<Entry> = Vec::new();
    let mut parent_id: Option<String> = None;
    for index in 0..10 {
        let user = message_entry(
            (index * 2) as u64,
            user_message(&format!("User {index}")),
            parent_id.clone(),
        );
        parent_id = Some(user.id().to_string());
        let assistant = message_entry(
            (index * 2 + 1) as u64,
            assistant_message(
                &format!("Assistant {index}"),
                mock_usage(0, 100, (index as u64 + 1) * 1000, 0),
            ),
            parent_id.clone(),
        );
        parent_id = Some(assistant.id().to_string());
        entries.push(user);
        entries.push(assistant);
    }

    let result = find_cut_point(&entries, 0, entries.len(), 2500);
    assert!(matches!(
        entries[result.first_kept_entry_index],
        Entry::Message { .. }
    ));
}

#[test]
fn covers_cut_point_and_turn_start_edge_cases() {
    let thinking = thinking_entry(1, "high", None);
    let model_change = model_change_entry(2, "openai", "gpt-4", Some(thinking.id().to_string()));
    assert_eq!(
        find_cut_point(&[thinking.clone(), model_change.clone()], 0, 2, 1),
        CutPointResult {
            first_kept_entry_index: 0,
            turn_start_index: -1,
            is_split_turn: false,
        }
    );

    let branch_summary = Entry::BranchSummary {
        id: "entry-3".to_string(),
        seq: 3,
        parent_id: Some(model_change.id().to_string()),
        timestamp: 1,
        from_id: "branch".to_string(),
        summary: "branch summary".to_string(),
        details: None,
        usage: None,
    };
    assert_eq!(
        find_turn_start_index(&[thinking.clone(), branch_summary.clone()], 1, 0),
        1
    );
    assert_eq!(find_turn_start_index(&[thinking, model_change], 1, 0), -1);

    let result = find_cut_point(&[thinking_entry(1, "high", None), branch_summary], 0, 2, 1);
    assert_eq!(result.first_kept_entry_index, 0);

    let tool_result = message_entry(
        1,
        AgentMessage::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
            role: Default::default(),
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: "tool output".to_string(),
                ..Default::default()
            })],
            is_error: false,
            timestamp: 1,
            ..Default::default()
        })),
        None,
    );
    assert_eq!(
        find_cut_point(&[tool_result], 0, 1, 1),
        CutPointResult {
            first_kept_entry_index: 0,
            turn_start_index: -1,
            is_split_turn: false,
        }
    );

    let user = message_entry(1, user_message("user"), None);
    let compaction = compaction_entry(2, "summary", Some(user.id().to_string()), Vec::new());
    let assistant = message_entry(
        3,
        assistant_message("assistant", mock_usage(100, 50, 0, 0)),
        Some(compaction.id().to_string()),
    );
    let result = find_cut_point(&[user, compaction, assistant], 0, 3, 1);
    assert_eq!(result.first_kept_entry_index, 2);
}

#[test]
fn estimates_tokens_across_supported_message_roles() {
    // user text: ceil(chars/4)
    assert_eq!(estimate_tokens(&user_message("123456789")), 3);
    // assistant text + thinking + tool call name/arguments
    let assistant = AgentMessage::Assistant(Box::new(pi_core::ai::types::AssistantMessage {
        role: RoleAssistant,
        content: vec![
            AssistantContent::Thinking(pi_core::ai::types::ThinkingContent {
                thinking: "abcd".to_string(),
                ..Default::default()
            }),
            AssistantContent::Text(TextContent {
                text: "1234".to_string(),
                ..Default::default()
            }),
            AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
                id: "call".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "ls" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ..Default::default()
            }),
        ],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        usage: mock_usage(10, 5, 3, 2),
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }));
    let tokens = estimate_tokens(&assistant);
    // thinking(4) + text(4) + name(4) + args length — all over 4
    assert!(tokens >= 3, "unexpected estimate {tokens}");

    // custom roles ride JSON payloads
    let custom = pi_core::agent::harness::messages::custom_message(
        "bashExecution",
        json!({ "role": "bashExecution", "command": "1234", "output": "1234", "timestamp": 1 }),
    );
    assert_eq!(estimate_tokens(&custom), 2);
}

#[test]
fn estimates_context_tokens_with_usage_anchor() {
    let messages = vec![
        user_message("one"),
        assistant_message("two", mock_usage(1000, 500, 0, 0)),
        user_message("three four five six seven"),
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1500);
    assert_eq!(estimate.last_used_index(), Some(1));
    // trailing tokens estimated by chars/4 (24 chars → 6)
    assert_eq!(estimate.trailing_tokens, 7);
    assert_eq!(estimate.tokens, 1507);

    let no_usage = vec![user_message("one"), user_message("two")];
    let estimate = estimate_context_tokens(&no_usage);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.last_used_index(), None);
}

impl ContextUsageEstimateExt
    for pi_core::agent::harness::compaction::compaction::ContextUsageEstimate
{
    fn last_used_index(&self) -> Option<usize> {
        self.last_usage_index
    }
}

trait ContextUsageEstimateExt {
    fn last_used_index(&self) -> Option<usize>;
}

#[test]
fn builds_session_context_with_a_compaction_entry() {
    let entries = vec![
        message_entry(1, user_message("old"), None),
        compaction_entry(
            2,
            "summary",
            Some("entry-1".to_string()),
            vec![user_message("retained")],
        ),
        message_entry(3, user_message("tail"), Some("entry-2".to_string())),
    ];
    let context = build_session_context(&entries, &Default::default());
    let roles: Vec<&str> = context.messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, ["compactionSummary", "user", "user"]);
    assert_eq!(context.thinking_level, "off");
}

#[test]
fn tracks_model_and_thinking_level_changes_in_built_context() {
    let entries = vec![
        thinking_entry(1, "high", None),
        model_change_entry(2, "openai", "gpt-5", Some("entry-1".to_string())),
        message_entry(3, user_message("hi"), Some("entry-2".to_string())),
    ];
    let context = build_session_context(&entries, &Default::default());
    assert_eq!(context.thinking_level, "high");
    let model = context.model.expect("model");
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model_id, "gpt-5");
}

#[test]
fn prepares_compaction_using_the_latest_summary_as_previous_summary() {
    let entries = vec![
        message_entry(1, user_message("user msg 1"), None),
        message_entry(
            2,
            assistant_message("assistant msg 1", mock_usage(100, 50, 0, 0)),
            Some("entry-1".to_string()),
        ),
        message_entry(3, user_message("user msg 2"), Some("entry-2".to_string())),
        message_entry(
            4,
            assistant_message("assistant msg 2", mock_usage(5000, 1000, 0, 0)),
            Some("entry-3".to_string()),
        ),
        compaction_entry(5, "First summary", Some("entry-4".to_string()), Vec::new()),
        message_entry(6, user_message("user msg 3"), Some("entry-5".to_string())),
        message_entry(
            7,
            assistant_message("assistant msg 3", mock_usage(8000, 2000, 0, 0)),
            Some("entry-6".to_string()),
        ),
    ];
    let preparation = prepare_compaction(&entries, DEFAULT_COMPACTION_SETTINGS)
        .expect("preparation")
        .expect("applicable");
    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("First summary")
    );
    assert!(!preparation.retained_tail.is_empty());
    let expected_tokens =
        estimate_context_tokens(&build_session_context(&entries, &Default::default()).messages)
            .tokens;
    assert_eq!(preparation.tokens_before, expected_tokens);
}

#[test]
fn carries_a_previous_compactions_retained_tail_into_the_next_preparation() {
    let retained_user = user_message("retained user");
    let retained_assistant = assistant_message("retained assistant", mock_usage(100, 50, 0, 0));
    let compaction = compaction_entry(
        1,
        "previous summary",
        None,
        vec![retained_user.clone(), retained_assistant.clone()],
    );
    let user = message_entry(2, user_message("new user"), Some("entry-1".to_string()));
    let assistant = message_entry(
        3,
        assistant_message("new assistant", mock_usage(100, 50, 0, 0)),
        Some("entry-2".to_string()),
    );

    let preparation = prepare_compaction(
        &[compaction, user, assistant],
        CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        },
    )
    .expect("preparation")
    .expect("applicable");
    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("previous summary")
    );
    let mut combined = preparation.messages_to_summarize.clone();
    combined.extend(preparation.turn_prefix_messages.clone());
    combined.extend(preparation.retained_tail.clone());
    let roles: Vec<&str> = combined.iter().map(|m| m.role()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
}

#[test]
fn does_not_prepare_compaction_when_nothing_valid_to_compact() {
    let entries = vec![compaction_entry(1, "only compaction", None, Vec::new())];
    let preparation = prepare_compaction(&entries, DEFAULT_COMPACTION_SETTINGS).unwrap();
    assert!(preparation.is_none());

    let preparation = prepare_compaction(&[], DEFAULT_COMPACTION_SETTINGS).unwrap();
    assert!(preparation.is_none());
}

#[test]
fn serializes_conversation_with_truncated_tool_results() {
    let messages = vec![
        pi_core::ai::types::Message::User(UserMessage {
            role: RoleUser,
            content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
                text: "hello".to_string(),
                ..Default::default()
            })]),
            timestamp: 1,
        }),
        pi_core::ai::types::Message::ToolResult(Box::new(pi_core::ai::types::ToolResultMessage {
            role: Default::default(),
            tool_call_id: "call".to_string(),
            tool_name: "read".to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: "x".repeat(3000),
                ..Default::default()
            })],
            is_error: false,
            timestamp: 1,
            ..Default::default()
        })),
    ];
    let serialized = serialize_conversation(&messages);
    assert!(serialized.starts_with("[User]: hello\n\n[Tool result]: "));
    assert!(
        serialized.contains("[... 1000 more characters truncated]"),
        "{serialized}"
    );
}

// ---------------------------------------------------------------------------
// branch-summarization.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collects_the_abandoned_side_of_a_branch_in_chronological_order() {
    let (session, ids) = session_with_entries_sync().await;
    // ids: root, common, abandoned 1, abandoned 2 (main lane). Fork a
    // target lane from the "common" entry.
    let common_id = ids[1].clone();
    session
        .create_lane("target", Some(common_id.clone()))
        .await
        .unwrap();
    let target_id = append_on_lane(&session, "target").await;

    let result = collect_entries_for_branch_summary(&session, Some(ids[3].as_str()), &target_id)
        .await
        .unwrap();
    assert_eq!(
        result.common_ancestor_id.as_deref(),
        Some(common_id.as_str())
    );
    let collected: Vec<&str> = result.entries.iter().map(|entry| entry.id()).collect();
    assert_eq!(collected, [ids[2].as_str(), ids[3].as_str()]);
    assert!(!collected.contains(&ids[0].as_str()));
}

async fn append_on_lane(session: &Session, lane: &str) -> String {
    session
        .append_entry(
            Entry::Message {
                id: "pending".to_string(),
                seq: 0,
                parent_id: None,
                timestamp: 0,
                message: user_message("target"),
                terminate: None,
            },
            lane,
        )
        .await
        .expect("lane append")
        .id()
        .to_string()
}

#[tokio::test]
async fn returns_no_entries_when_there_was_no_previous_leaf() {
    let (session, ids) = session_with_entries_sync().await;
    let result = collect_entries_for_branch_summary(&session, None, &ids[0])
        .await
        .unwrap();
    assert!(result.entries.is_empty());
    assert_eq!(result.common_ancestor_id, None);
}

#[tokio::test]
async fn generates_branch_summary_through_the_faux_provider() {
    let faux = faux_provider(RegisterFauxProviderOptions {
        ..Default::default()
    });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("branch summary text", FauxMessageOptions::default()),
    ))]);

    let entries = vec![message_entry(1, user_message("explored a thing"), None)];
    let result = generate_branch_summary(
        &entries,
        &GenerateBranchSummaryOptions {
            models: Arc::clone(&models),
            model: faux.get_model(),
            signal: None,
            custom_instructions: None,
            replace_instructions: false,
            reserve_tokens: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap();

    assert!(
        result.summary.starts_with(
            "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\nbranch summary text"
        ),
        "unexpected summary: {}",
        result.summary
    );
}

async fn session_with_entries_sync() -> (Session, Vec<String>) {
    let storage = InMemorySessionStorage::new(SessionMetadata {
        id: "session".to_string(),
        created_at: 1,
        parent_session_id: None,
    });
    let next_id = std::sync::atomic::AtomicU32::new(0);
    let session = Session::with_id_generator(
        Arc::new(storage),
        std::sync::Arc::new(move || {
            format!(
                "entry-{}",
                next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            )
        }),
    );
    let mut ids = Vec::new();
    for text in ["root", "common", "abandoned 1", "abandoned 2"] {
        let id = session
            .append_message(user_message(text))
            .await
            .expect("append");
        ids.push(id);
    }
    (session, ids)
}

// ---------------------------------------------------------------------------
// compaction.test.ts — faux-provider summary suites
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Port of `createModelsWithSimpleResponses`: a provider whose completeSimple
// returns the queued messages verbatim (the TS test stubs `completeSimple`
// on the Models object to bypass faux usage estimation).

struct StubCompleteStreams {
    remaining: Mutex<std::collections::VecDeque<AiAssistantMessage>>,
}

impl pi_core::ai::models::ProviderStreams for StubCompleteStreams {
    fn stream(
        &self,
        model: &pi_core::ai::types::Model,
        context: &Context,
        options: Option<&pi_core::ai::types::StreamOptions>,
    ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
        let simple = options.map(|options| SimpleStreamOptions {
            base: options.clone(),
            ..Default::default()
        });
        self.stream_simple(model, context, simple.as_ref())
    }

    fn stream_simple(
        &self,
        _model: &pi_core::ai::types::Model,
        _context: &Context,
        _options: Option<&SimpleStreamOptions>,
    ) -> pi_core::ai::utils::event_stream::AssistantMessageEventStream {
        let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
        let next = self.remaining.lock().unwrap().pop_front();
        let producer = stream.clone();
        tokio::spawn(async move {
            match next {
                Some(message) => {
                    producer.push(pi_core::ai::types::AssistantMessageEvent::Start {
                        partial: message.clone(),
                    });
                    producer.push(pi_core::ai::types::AssistantMessageEvent::Done {
                        reason: pi_core::ai::types::DoneReason::Stop,
                        message,
                    });
                    producer.end(None);
                }
                None => {
                    producer.push(pi_core::ai::types::AssistantMessageEvent::Error {
                        reason: pi_core::ai::types::ErrorReason::Error,
                        error: pi_core::ai::types::AssistantMessage {
                            role: RoleAssistant,
                            content: Vec::new(),
                            api: "stub-complete".to_string(),
                            provider: "stub-compaction".to_string(),
                            model: "non-reasoning-model".to_string(),
                            usage: Default::default(),
                            stop_reason: StopReason::Error,
                            timestamp: 0,
                            error_message: Some(
                                "No faux completeSimple response queued".to_string(),
                            ),
                            ..Default::default()
                        },
                    });
                    producer.end(None);
                }
            }
        });
        stream
    }
}

struct StubApiKeyAuth;

impl pi_core::ai::auth::types::ApiKeyAuth for StubApiKeyAuth {
    fn name(&self) -> &str {
        "Stub API key"
    }

    fn resolve(
        &self,
        _input: pi_core::ai::auth::types::ApiKeyAuthInput,
    ) -> pi_core::ai::auth::types::AuthFuture<
        Result<
            Option<pi_core::ai::auth::types::AuthResult>,
            pi_core::ai::auth::types::AuthStorageError,
        >,
    > {
        Box::pin(async {
            Ok(Some(pi_core::ai::auth::types::AuthResult {
                auth: pi_core::ai::auth::types::ModelAuth::default(),
                env: None,
                source: None,
            }))
        })
    }
}

fn stub_models_with_simple_responses(
    responses: Vec<AiAssistantMessage>,
) -> (Arc<Models>, pi_core::ai::types::Model) {
    let streams = Arc::new(StubCompleteStreams {
        remaining: Mutex::new(responses.into_iter().collect()),
    });
    let model = pi_core::ai::types::Model {
        id: "non-reasoning-model".to_string(),
        name: "Non-reasoning stub model".to_string(),
        api: "stub-complete".to_string(),
        provider: "stub-compaction".to_string(),
        reasoning: false,
        input: vec![pi_core::ai::types::ModelInput::Text],
        cost: pi_core::ai::types::ModelCost::default(),
        context_window: 200_000,
        max_tokens: 8_192,
        ..Default::default()
    };
    let provider = Arc::new(pi_core::ai::models::BasicProvider::new(
        pi_core::ai::models::CreateProviderOptions {
            id: "stub-compaction".to_string(),
            organization_id: None,
            name: None,
            base_url: None,
            headers: None,
            auth: pi_core::ai::auth::types::ProviderAuth::api_key(Arc::new(StubApiKeyAuth)),
            models: vec![model.clone()],
            fetch_models: None,
            filter_models: None,
            api: pi_core::ai::models::ProviderApi::Single(streams),
        },
    ));
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(provider);
    (models, model)
}

static FAUX_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Port of `createFauxModel`.
fn create_faux_model(
    reasoning: bool,
    max_tokens: u64,
) -> (
    pi_core::ai::providers::faux::FauxProviderHandle,
    Arc<Models>,
) {
    let count = FAUX_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let faux = faux_provider(RegisterFauxProviderOptions {
        provider: Some(format!("faux-compaction-{count}")),
        models: vec![pi_core::ai::providers::faux::FauxModelDefinition {
            id: if reasoning {
                "reasoning-model".to_string()
            } else {
                "non-reasoning-model".to_string()
            },
            name: None,
            reasoning: Some(reasoning),
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(max_tokens),
        }],
        ..Default::default()
    });
    let models = Arc::new(Models::new(Default::default()));
    models.set_provider(Arc::clone(&faux.provider));
    (faux, models)
}

fn default_faux_model() -> (
    pi_core::ai::providers::faux::FauxProviderHandle,
    Arc<Models>,
) {
    create_faux_model(false, 8192)
}

fn summary_options() -> pi_core::agent::harness::compaction::compaction::SummaryOptions<'static> {
    pi_core::agent::harness::compaction::compaction::SummaryOptions {
        signal: None,
        custom_instructions: None,
        previous_summary: None,
        thinking_level: None,
        retry: None,
        callbacks: None,
    }
}

#[tokio::test]
async fn passes_reasoning_through_generate_summary_for_reasoning_models_with_thinking_enabled() {
    let messages = vec![user_message("Summarize this.")];
    let seen_reasoning: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));

    for (reasoning, thinking, expected) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
    ] {
        let (faux, models) = create_faux_model(reasoning, 8192);
        let seen = Arc::clone(&seen_reasoning);
        faux.set_responses(vec![FauxResponseStep::Factory(Arc::new(
            move |_context, options: Option<&SimpleStreamOptions>, _state, _model| {
                seen.lock()
                    .unwrap()
                    .push(options.and_then(|options| options.reasoning).is_some());
                Ok(faux_assistant_message(
                    "## Goal\nTest summary",
                    FauxMessageOptions::default(),
                ))
            },
        ))]);
        let options = pi_core::agent::harness::compaction::compaction::SummaryOptions {
            thinking_level: if thinking {
                Some(pi_core::agent::types::ThinkingLevel::Medium)
            } else {
                None
            },
            ..summary_options()
        };
        let result = pi_core::agent::harness::compaction::compaction::generate_summary(
            &messages,
            &models,
            &faux.get_model(),
            2000,
            &options,
        )
        .await;
        assert_eq!(result.unwrap(), "## Goal\nTest summary");
        let _ = expected;
    }

    let seen = seen_reasoning.lock().unwrap().clone();
    assert_eq!(seen, vec![true, false, false]);
}

#[tokio::test]
async fn includes_previous_summaries_and_custom_instructions_in_prompts() {
    let messages = vec![user_message("Summarize this.")];
    let (faux, models) = default_faux_model();
    let prompt_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let prompt = Arc::clone(&prompt_text);
    faux.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        move |context: &Context, _options, _state, _model| {
            let text = context
                .messages
                .first()
                .and_then(|message| match message {
                    Message::User(user) => match &user.content {
                        UserContent::Text(text) => Some(text.clone()),
                        UserContent::Blocks(blocks) => {
                            blocks.iter().find_map(|block| match block {
                                pi_core::ai::types::BlockContent::Text(text) => {
                                    Some(text.text.clone())
                                }
                                _ => None,
                            })
                        }
                    },
                    _ => None,
                })
                .unwrap_or_default();
            *prompt.lock().unwrap() = text;
            let mut message = faux_assistant_message("Test summary", FauxMessageOptions::default());
            message.usage = mock_usage(3, 4, 0, 0);
            Ok(message)
        },
    ))]);

    let options = pi_core::agent::harness::compaction::compaction::SummaryOptions {
        custom_instructions: Some("focus"),
        previous_summary: Some("old summary"),
        ..summary_options()
    };
    let (summary, usage) =
        pi_core::agent::harness::compaction::compaction::generate_summary_with_usage(
            &messages,
            &models,
            &faux.get_model(),
            2000,
            &options,
        )
        .await
        .unwrap();

    assert!(summary.contains("Test summary"));
    assert!(usage.input > 0 && usage.output > 0);
    assert_eq!(
        usage.total_tokens,
        usage.input + usage.output + usage.cache_read + usage.cache_write
    );
    let prompt = prompt_text.lock().unwrap().clone();
    assert!(
        prompt.contains("<previous-summary>\nold summary\n</previous-summary>"),
        "missing previous summary block: {prompt}"
    );
    assert!(
        prompt.contains("Additional focus: focus"),
        "missing custom instruction: {prompt}"
    );
}

#[tokio::test]
async fn preserves_the_string_result_from_generate_summary() {
    let messages = vec![user_message("Summarize this.")];
    let (faux, models) = default_faux_model();
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("## Goal\nTest summary", FauxMessageOptions::default()),
    ))]);

    let summary = pi_core::agent::harness::compaction::compaction::generate_summary(
        &messages,
        &models,
        &faux.get_model(),
        2000,
        &summary_options(),
    )
    .await
    .unwrap();

    assert_eq!(summary, "## Goal\nTest summary");
}

#[tokio::test]
async fn returns_error_results_for_failed_or_aborted_summary_generations() {
    let messages = vec![user_message("Summarize this.")];

    let (error_faux, error_models) = default_faux_model();
    error_faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "",
            FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("boom".to_string()),
                ..Default::default()
            },
        ),
    ))]);
    let error = pi_core::agent::harness::compaction::compaction::generate_summary(
        &messages,
        &error_models,
        &error_faux.get_model(),
        2000,
        &summary_options(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (
            CompactionErrorCode::SummarizationFailed,
            "Summarization failed: boom"
        )
    );

    let (aborted_faux, aborted_models) = default_faux_model();
    aborted_faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "",
            FauxMessageOptions {
                stop_reason: Some(StopReason::Aborted),
                error_message: Some("stopped".to_string()),
                ..Default::default()
            },
        ),
    ))]);
    let aborted = pi_core::agent::harness::compaction::compaction::generate_summary(
        &messages,
        &aborted_models,
        &aborted_faux.get_model(),
        2000,
        &summary_options(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (aborted.code, aborted.message.as_str()),
        (CompactionErrorCode::Aborted, "stopped")
    );
}

#[tokio::test]
async fn clamps_compaction_summary_max_tokens_to_the_model_output_cap() {
    let messages = vec![user_message("Summarize this.")];
    let (faux, models) = create_faux_model(false, 128_000);
    let seen_options: Arc<Mutex<Vec<SimpleStreamOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_options);
    let factory: pi_core::ai::providers::faux::FauxResponseFactory = Arc::new(
        move |_context, options: Option<&SimpleStreamOptions>, _state, _model| {
            seen.lock()
                .unwrap()
                .push(options.cloned().unwrap_or_default());
            Ok(faux_assistant_message(
                "## Goal\nTest summary",
                FauxMessageOptions::default(),
            ))
        },
    );
    faux.set_responses(vec![
        FauxResponseStep::Factory(Arc::clone(&factory)),
        FauxResponseStep::Factory(factory),
    ]);

    let preparation = pi_core::agent::harness::compaction::compaction::CompactionPreparation {
        messages_to_summarize: messages.clone(),
        turn_prefix_messages: messages,
        retained_tail: Vec::new(),
        is_split_turn: true,
        tokens_before: 600_000,
        previous_summary: None,
        file_ops: Default::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 500_000,
            keep_recent_tokens: 20_000,
        },
    };
    pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap();

    let seen = seen_options.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for options in seen.iter() {
        assert_eq!(options.base.max_tokens, Some(128_000));
        assert_eq!(options.base.cache_retention, Some(CacheRetention::None));
    }
    assert_ne!(seen[0].base.session_id, seen[1].base.session_id);
}

#[tokio::test]
async fn returns_compaction_error_results_without_throwing() {
    let messages = vec![user_message("Summarize this.")];
    let (faux, models) = default_faux_model();
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "",
            FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("history failed".to_string()),
                ..Default::default()
            },
        ),
    ))]);
    let preparation = pi_core::agent::harness::compaction::compaction::CompactionPreparation {
        messages_to_summarize: messages,
        turn_prefix_messages: Vec::new(),
        retained_tail: Vec::new(),
        is_split_turn: false,
        tokens_before: 100,
        previous_summary: None,
        file_ops: Default::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };

    let error = pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (
            CompactionErrorCode::SummarizationFailed,
            "Summarization failed: history failed"
        )
    );
}

#[tokio::test]
async fn combines_usage_for_split_turn_compaction_summaries() {
    let messages = vec![user_message("Summarize this.")];
    // Two scripted responses with distinct usage: history then prefix.
    let mut history = faux_assistant_message("history summary", FauxMessageOptions::default());
    history.usage = mock_usage(1, 2, 3, 4);
    let mut prefix = faux_assistant_message("turn prefix summary", FauxMessageOptions::default());
    prefix.usage = mock_usage(5, 6, 7, 8);
    let (models, model) = stub_models_with_simple_responses(vec![history, prefix]);

    let preparation = pi_core::agent::harness::compaction::compaction::CompactionPreparation {
        messages_to_summarize: messages.clone(),
        turn_prefix_messages: messages,
        retained_tail: Vec::new(),
        is_split_turn: true,
        tokens_before: 100,
        previous_summary: None,
        file_ops: Default::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };
    let result = pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &model,
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(result.usage, mock_usage(6, 8, 10, 12));
}

#[tokio::test]
async fn passes_reasoning_through_turn_prefix_summaries_when_enabled() {
    let messages = vec![user_message("Summarize this.")];
    let (faux, models) = create_faux_model(true, 8192);
    let seen_reasoning: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_reasoning);
    faux.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        move |_context, options: Option<&SimpleStreamOptions>, _state, _model| {
            seen.lock()
                .unwrap()
                .push(options.and_then(|options| options.reasoning).is_some());
            Ok(faux_assistant_message(
                "## Original Request\nTest summary",
                FauxMessageOptions::default(),
            ))
        },
    ))]);
    let preparation = pi_core::agent::harness::compaction::compaction::CompactionPreparation {
        messages_to_summarize: Vec::new(),
        turn_prefix_messages: messages,
        retained_tail: Vec::new(),
        is_split_turn: true,
        tokens_before: 100,
        previous_summary: None,
        file_ops: Default::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };
    pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: Some(pi_core::agent::types::ThinkingLevel::High),
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(seen_reasoning.lock().unwrap().clone(), vec![true]);
}

#[tokio::test]
async fn returns_turn_prefix_compaction_errors_without_throwing() {
    let messages = vec![user_message("Summarize this.")];
    let preparation = pi_core::agent::harness::compaction::compaction::CompactionPreparation {
        messages_to_summarize: Vec::new(),
        turn_prefix_messages: messages,
        retained_tail: Vec::new(),
        is_split_turn: true,
        tokens_before: 100,
        previous_summary: None,
        file_ops: Default::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };

    let (faux, models) = default_faux_model();
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "",
            FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("prefix failed".to_string()),
                ..Default::default()
            },
        ),
    ))]);
    let error = pi_core::agent::harness::compaction::compaction::compact(
        preparation.clone(),
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (
            CompactionErrorCode::SummarizationFailed,
            "Turn prefix summarization failed: prefix failed"
        )
    );

    let (aborted_faux, aborted_models) = default_faux_model();
    aborted_faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message(
            "",
            FauxMessageOptions {
                stop_reason: Some(StopReason::Aborted),
                error_message: Some("prefix stopped".to_string()),
                ..Default::default()
            },
        ),
    ))]);
    let aborted = pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &aborted_models,
            model: &aborted_faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        (aborted.code, aborted.message.as_str()),
        (CompactionErrorCode::Aborted, "prefix stopped")
    );
}

#[test]
fn prepares_split_turn_compaction_with_prior_file_operation_details() {
    let u1 = message_entry(1, user_message("user msg 1"), None);
    let assistant = assistant_message_with_content(
        vec![AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
            id: "tool-1".to_string(),
            name: "write".to_string(),
            arguments: json!({"path": "written.ts"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ..Default::default()
        })],
        mock_usage(100, 50, 0, 0),
    );
    let a1 = message_entry(2, assistant, Some(u1.id().to_string()));
    let mut compaction1 =
        compaction_entry(3, "First summary", Some(a1.id().to_string()), Vec::new());
    if let Entry::Compaction { details, .. } = &mut compaction1 {
        *details = Some(json!({
            "readFiles": ["old-read.ts"],
            "modifiedFiles": ["old-edit.ts", "written.ts"],
        }));
    }
    let u2 = message_entry(
        4,
        user_message("large turn"),
        Some(compaction1.id().to_string()),
    );
    let a2 = message_entry(
        5,
        assistant_message("large assistant message", mock_usage(100, 50, 0, 0)),
        Some(u2.id().to_string()),
    );

    let preparation = prepare_compaction(
        &[u1, a1, compaction1, u2, a2],
        CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        },
    )
    .unwrap()
    .expect("preparation");

    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("First summary")
    );
    assert!(preparation.is_split_turn);
    let prefix_roles: Vec<&str> = preparation
        .turn_prefix_messages
        .iter()
        .map(|message| match message {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::Custom(_) => "custom",
        })
        .collect();
    assert_eq!(prefix_roles, vec!["user"]);
    let read: Vec<&String> = preparation.file_ops.read.iter().collect();
    assert!(read.contains(&&"old-read.ts".to_string()));
    let edited: Vec<&String> = preparation.file_ops.edited.iter().collect();
    assert!(edited.contains(&&"old-edit.ts".to_string()));
    assert!(edited.contains(&&"written.ts".to_string()));
}

#[tokio::test]
async fn returns_a_compaction_result_with_file_details() {
    let u1 = message_entry(1, user_message("read a file"), None);
    let assistant = assistant_message_with_content(
        vec![AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
            id: "tool-1".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "src/index.ts"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            ..Default::default()
        })],
        mock_usage(1000, 200, 0, 0),
    );
    let a1 = message_entry(2, assistant, Some(u1.id().to_string()));
    let u2 = message_entry(3, user_message("continue"), Some(a1.id().to_string()));
    let a2 = message_entry(
        4,
        assistant_message("done", mock_usage(4000, 500, 0, 0)),
        Some(u2.id().to_string()),
    );

    let preparation = prepare_compaction(&[u1, a1, u2, a2], DEFAULT_COMPACTION_SETTINGS)
        .unwrap()
        .expect("preparation");
    let (faux, models) = default_faux_model();
    faux.set_responses(vec![FauxResponseStep::Message(Box::new(
        faux_assistant_message("## Goal\nTest summary", FauxMessageOptions::default()),
    ))]);

    let result = pi_core::agent::harness::compaction::compaction::compact(
        preparation,
        &pi_core::agent::harness::compaction::compaction::CompactOptions {
            models: &models,
            model: &faux.get_model(),
            custom_instructions: None,
            signal: None,
            thinking_level: None,
            retry: None,
            callbacks: None,
        },
    )
    .await
    .unwrap();

    assert!(!result.summary.is_empty());
    assert!(result.usage.total_tokens > 0);
    assert!(!result.retained_tail.is_empty());
    let _ = &result.details; // the TS case asserts the details object exists.
}
