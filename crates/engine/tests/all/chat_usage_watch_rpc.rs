//! Handle-seam tests for the chat usage frame (`WatchChatUsage`, spec ticket
//! 04). Four contracts live here: the occupancy numerator is the latest
//! main-run provider report — its request input plus both cache fields, and
//! never the run accumulator that the same message also feeds — the
//! denominator is the window of the model the chat's queue runs next (the
//! chat's own selection only while the queue holds nothing, and absent when
//! that window is unknown to the catalog, as a custom model's is), and the
//! frame's record count is the runtime's own replay state rather than a
//! fresh read of the ledger file; the Context report mirrors the latest
//! main request in order and starts empty after a restart.

use crate::common;

use std::sync::Arc;

use common::{
    Fixture, ScriptedProvider, ScriptedReply, next_frame, run_prompt, setup_chat,
    wait_for_requests, wait_for_session_status,
};
use futures::StreamExt;
use holt_core::ai::types::{Message, Usage};
use holt_rpc::{RpcReply, RpcService as _, methods};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// One frame off the usage watch, pumping until `ready` accepts one — the
/// harness's timeout bounds a frame that never arrives.
async fn frame_until<S>(stream: &mut S, ready: impl Fn(&Value) -> bool) -> Value
where
    S: StreamExt<Item = Value> + Unpin,
{
    loop {
        let frame = next_frame(stream).await;
        if ready(&frame) {
            return frame;
        }
    }
}

async fn usage_watch(engine: &holt_engine::LocalEngine) -> impl StreamExt<Item = Value> + Unpin {
    let RpcReply::Stream(stream) = engine
        .handle(methods::WATCH_CHAT_USAGE, json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("WatchChatUsage did not return a stream");
    };
    stream
}

/// The window the engine's own catalog reports for a model — the tests never
/// hardcode a catalog number.
async fn window_of(engine: &holt_engine::LocalEngine, model_id: &str) -> Option<u64> {
    let RpcReply::Value(models) = engine
        .handle(methods::LIST_MODELS, json!({"providerId": "openai"}))
        .await
        .unwrap()
    else {
        panic!("ListModels did not return a value");
    };
    models
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == model_id)
        .unwrap_or_else(|| panic!("no catalog row for {model_id}"))["contextWindow"]
        .as_u64()
}

/// Set the chat's selected model exactly as the model picker does.
async fn select_model(engine: &holt_engine::LocalEngine, model: &str) {
    select_model_on(engine, "chat-1", model).await;
}

/// `select_model` for a chat the shared helper does not hardcode.
async fn select_model_on(engine: &holt_engine::LocalEngine, chat_id: &str, model: &str) {
    engine
        .handle(
            methods::MUTATE,
            json!({"op": "setChatConfig", "chatId": chat_id, "config": {
                "provider": "openai",
                "model": model,
                "reasoning": null,
                "modelOptions": {},
            }}),
        )
        .await
        .unwrap();
}

/// Queue one run with an explicit model — the composer's own shape, and the
/// only way a queue item's model can differ from the chat's selection.
async fn queue_run_on(
    engine: &holt_engine::LocalEngine,
    cwd: &str,
    message_id: &str,
    model: &str,
    prompt: &str,
) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "run",
                    "messageId": message_id,
                    "request": {
                        "prompt": prompt,
                        "provider": "openai",
                        "model": model,
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": cwd,
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn occupancy_is_the_latest_reports_request_input_not_the_run_accumulator() {
    let fixture = Fixture::new();
    // Distinct in every field, so a doubled numerator (the report counted
    // once by the ledger and once by the run accumulator) stands out from
    // the honest 700 + 7 + 3.
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]).with_usage(Usage {
        input: 700,
        output: 70,
        cache_read: 7,
        cache_write: 3,
        total_tokens: 780,
        ..Default::default()
    });
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let mut usage = usage_watch(&engine).await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    let frame = frame_until(&mut usage, |frame| frame["gross"] == json!(780)).await;
    assert_eq!(frame["occupancy"]["tokens"], json!(710), "{frame}");
    assert_eq!(frame["occupancy"]["estimated"], json!(false), "{frame}");
}

/// The report mirrors the exact converted main-loop Context, including a
/// finished tool round and the next Turn's verbatim History order.
#[tokio::test]
async fn context_report_mirrors_the_latest_main_request_in_order() {
    let fixture = Fixture::new();
    std::fs::write(
        std::path::Path::new(&fixture.cwd()).join("notes.txt"),
        "context file body",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("call-1", "read", json!({ "path": "notes.txt" })),
        ScriptedReply::text("first answer"),
        ScriptedReply::text("second answer"),
    ]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    let mut usage = usage_watch(&engine).await;

    run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let frame = frame_until(&mut usage, |frame| {
        frame["context"]["entries"]
            .as_array()
            .is_some_and(|entries| {
                entries
                    .last()
                    .is_some_and(|entry| entry["content"] == json!("second prompt"))
            })
    })
    .await;
    let request = provider
        .requests()
        .into_iter()
        .rev()
        .find(|request| {
            request.messages.iter().any(|message| {
                matches!(message, Message::User(user) if user.content.text() == "second prompt")
            })
        })
        .expect("the main request reached the scripted provider");
    let entries = frame["context"]["entries"].as_array().unwrap();
    assert_eq!(frame["context"]["model"], json!(request.model));
    assert_eq!(entries[0]["kind"], json!("systemPrompt"));
    assert_eq!(
        entries[0]["content"],
        json!(request.system_prompt.clone().unwrap())
    );
    assert_eq!(entries[1]["kind"], json!("toolDefinitions"));
    let tools = entries[1]["children"].as_array().unwrap();
    assert_eq!(tools.len(), request.tools);
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        request.tool_names
    );

    let messages = &entries[2..];
    assert_eq!(messages.len(), request.messages.len());
    for (entry, message) in messages.iter().zip(&request.messages) {
        let expected_kind = match message {
            Message::User(_) => "userMessage",
            Message::Assistant(_) => "assistantMessage",
            Message::ToolResult(_) => "toolResult",
        };
        assert_eq!(entry["kind"], json!(expected_kind));
        assert!(entry["estimatedTokens"].as_u64().unwrap() > 0);
    }
    assert!(messages.iter().any(|entry| {
        entry["kind"] == json!("toolResult")
            && entry["name"] == json!("read")
            && entry["content"]
                .as_str()
                .unwrap()
                .contains("context file body")
    }));
    assert!(messages.iter().any(|entry| {
        entry["kind"] == json!("assistantMessage")
            && entry["content"].as_str().unwrap().contains("first answer")
    }));
    assert_eq!(messages.last().unwrap()["content"], json!("second prompt"));
}

/// A rate-limited round-trip never reaches a usage event, so its assistant
/// message arrives with an all-zero report. It books — the failed answer is
/// the Turn's own round-trip — but the occupancy must keep the previous
/// turn's measured number instead of collapsing to an empty context.
#[tokio::test]
async fn a_rate_limited_turn_keeps_the_previous_occupancy() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text_with_usage(
            "first",
            Usage {
                input: 700,
                output: 70,
                cache_read: 7,
                cache_write: 3,
                total_tokens: 780,
                ..Default::default()
            },
        ),
        ScriptedReply::Failed(
            "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"429\"}}"
                .into(),
        ),
    ])
    // The failed reply reports nothing, exactly as a pre-stream 429 does.
    .with_usage(Usage::default());
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let mut usage = usage_watch(&engine).await;
    let measured = frame_until(&mut usage, |frame| {
        frame["occupancy"]["estimated"] == json!(false)
    })
    .await;
    assert_eq!(measured["occupancy"]["tokens"], json!(710), "{measured}");

    run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    let failed = frame_until(&mut usage, |frame| frame["recordCount"] == json!(2)).await;
    // The zero record lands in the ledger without moving gross or occupancy.
    assert_eq!(failed["gross"], json!(780), "{failed}");
    assert_eq!(failed["occupancy"]["tokens"], json!(710), "{failed}");
    assert_eq!(failed["occupancy"]["estimated"], json!(false), "{failed}");
}

#[tokio::test]
async fn preview_publishes_basic_context_before_parallel_mcp_servers_finish() {
    let fixture = Fixture::new();
    let log = fixture.data_dir.path().join("list-log");
    let barrier = fixture.data_dir.path().join("list-barrier");
    let server = |name: &str, delay_ms: u64| {
        serde_json::json!({
            "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
            "env": {
                "FIXTURE_SERVER_ID": name,
                "FIXTURE_LIST_LOG": log.with_file_name(format!("{name}-log")),
                "FIXTURE_LIST_BARRIER": barrier,
                "FIXTURE_STALL_LIST_MS": delay_ms.to_string(),
            }
        })
    };
    std::fs::write(
        fixture.data_dir.path().join("mcp.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "mcpServers": {
                "slow": server("slow", 500),
                "fast": server("fast", 0),
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![]);
    let engine = Arc::new(fixture.engine(&provider));
    setup_chat(&engine, "chat-1").await;
    select_model(&engine, "openai/gpt-5.4").await;
    let mut usage = usage_watch(&engine).await;
    let for_preview = engine.clone();
    let preview_call = tokio::spawn(async move {
        for_preview
            .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId":"chat-1"}))
            .await
    });

    let base = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
            && frame["context"]["loadingMcpServers"]
                .as_array()
                .is_some_and(|servers| !servers.is_empty())
    })
    .await;
    assert!(
        base["context"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != json!("mcpServer")),
        "the basic preview does not wait for MCP definitions: {base}"
    );

    let full = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
            && frame["context"]["loadingMcpServers"].is_null()
    })
    .await;
    let groups: Vec<_> = full["context"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == json!("mcpServer"))
        .collect();
    assert_eq!(
        groups.len(),
        2,
        "each MCP server gets its own group: {full}"
    );
    assert!(groups.iter().any(|group| group["name"] == json!("fast")));
    assert!(groups.iter().any(|group| group["name"] == json!("slow")));
    assert_eq!(full["context"]["entries"][0], base["context"]["entries"][0]);
    preview_call.await.unwrap().unwrap();

    assert_eq!(
        std::fs::read_to_string(log.with_file_name("slow-log")).unwrap(),
        "slow:start\nslow:end\n"
    );
    assert_eq!(
        std::fs::read_to_string(log.with_file_name("fast-log")).unwrap(),
        "fast:start\nfast:end\n"
    );
    assert_eq!(
        std::fs::metadata(barrier).unwrap().len(),
        2,
        "both independent servers reached tools/list"
    );
}

#[tokio::test]
async fn context_report_groups_skills_without_changing_the_system_prompt() {
    let fixture = Fixture::new();
    let skill_dir = fixture.personal_dir.path().join("demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill.\n---\nDo not expose this body in the listing.\n",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    let mut usage = usage_watch(&engine).await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let frame = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(false)
            && frame["context"]["entries"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["kind"] == json!("skills")))
    })
    .await;
    let request = provider.requests().last().unwrap().clone();
    let system_prompt = request.system_prompt.unwrap();
    let entries = frame["context"]["entries"].as_array().unwrap();
    let system = entries
        .iter()
        .find(|entry| entry["kind"] == json!("systemPrompt"))
        .unwrap();
    assert_eq!(system["content"], json!(system_prompt));
    let skills = entries
        .iter()
        .find(|entry| entry["kind"] == json!("skills"))
        .unwrap();
    assert!(entries.iter().all(|entry| entry["kind"] != json!("skill")));
    let skill = &skills["children"][0];
    assert_eq!(skill["name"], json!("demo"));
    assert!(skill["content"].as_str().unwrap().contains("Demo skill."));
    assert!(system_prompt.contains(skill["content"].as_str().unwrap()));
}

#[tokio::test]
async fn occupancy_estimates_from_history_until_a_report_arrives_then_measures() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("first"),
        ScriptedReply::text("second"),
    ])
    .with_usage(Usage {
        input: 300,
        output: 30,
        cache_read: 5,
        cache_write: 2,
        total_tokens: 337,
        ..Default::default()
    });
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    drop(engine);

    // A restart has seen no report for this chat yet: the opening frame says
    // so and replays History through the estimator instead of pretending.
    let engine = fixture.engine(&provider);
    let mut usage = usage_watch(&engine).await;
    let estimated = next_frame(&mut usage).await;
    assert!(estimated["context"].is_null());
    assert_eq!(
        estimated["occupancy"]["estimated"],
        json!(true),
        "{estimated}"
    );
    assert!(
        estimated["occupancy"]["tokens"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "the estimate replays the persisted History: {estimated}"
    );
    assert_eq!(
        estimated["gross"],
        json!(337),
        "the ledger's total survives the restart even while occupancy falls back to an estimate: {estimated}"
    );

    // With no transport snapshot after restart, the same assembly path builds
    // a next-request preview from the restored History instead of an empty
    // report. The next real request must keep that preview as an exact prefix.
    let preview_reply = engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap();
    let RpcReply::Value(preview_reply) = preview_reply else {
        panic!("PreviewChatContext did not return a value")
    };
    assert_eq!(preview_reply["preview"], json!(true));
    let preview = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
    })
    .await;
    let preview_entries = preview["context"]["entries"].as_array().unwrap();
    assert_eq!(preview_entries[0]["kind"], json!("systemPrompt"));
    assert_eq!(preview_entries[1]["kind"], json!("toolDefinitions"));
    assert!(preview_entries.iter().any(|entry| {
        entry["kind"] == json!("userMessage") && entry["content"] == json!("first prompt")
    }));
    assert!(preview_entries.iter().any(|entry| {
        entry["kind"] == json!("assistantMessage")
            && entry["content"].as_str().unwrap().contains("first")
    }));

    // The first report of the process flips it to measured, and the number
    // is that report's own request input — cache fields included.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    let measured = frame_until(&mut usage, |frame| {
        frame["occupancy"]["estimated"] == json!(false)
            && frame["context"]["preview"] == json!(false)
    })
    .await;
    assert_eq!(measured["occupancy"]["tokens"], json!(307), "{measured}");
    let actual_entries = measured["context"]["entries"].as_array().unwrap();
    assert_eq!(actual_entries[0], preview_entries[0]);
    assert_eq!(actual_entries[1], preview_entries[1]);
    let preview_messages = &preview_entries[2..];
    let actual_messages = &actual_entries[2..];
    assert_eq!(actual_messages.len(), preview_messages.len() + 1);
    for (actual, preview) in actual_messages.iter().zip(preview_messages) {
        assert_eq!(actual["kind"], preview["kind"]);
        assert_eq!(actual["content"], preview["content"]);
    }
    assert_eq!(
        actual_messages.last().unwrap()["content"],
        json!("second prompt")
    );
}

#[tokio::test]
async fn the_denominator_follows_the_queue_head_then_the_selection() {
    let fixture = Fixture::new();
    let gate = Arc::new(Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;

    let selected_model = "openai/gpt-5.4";
    let queued_model = "openai/gpt-5.4-pro";
    let selected = window_of(&engine, selected_model)
        .await
        .expect("the catalog reports the selected model's window");
    let queued = window_of(&engine, queued_model)
        .await
        .expect("the catalog reports the queued model's window");
    assert_ne!(
        selected, queued,
        "the fixture needs two models with distinct windows"
    );

    let mut usage = usage_watch(&engine).await;
    // Nothing queued and no selection yet: the frame has no denominator, so
    // the UI falls back to absolute tokens instead of inventing a percentage.
    assert_eq!(
        next_frame(&mut usage).await["occupancy"]["contextWindow"],
        json!(null)
    );

    queue_run_on(&engine, &fixture.cwd(), "m-1", queued_model, "go").await;
    wait_for_requests(&provider, 1).await;
    // The run is running on the queued model; moving the picker elsewhere
    // must not move the denominator out from under it.
    select_model(&engine, selected_model).await;
    let frame = frame_until(&mut usage, |frame| {
        frame["occupancy"]["contextWindow"].as_u64() == Some(queued)
    })
    .await;
    assert_eq!(
        frame["occupancy"]["contextWindow"],
        json!(queued),
        "{frame}"
    );

    // The queue settles empty, so the chat's own selection answers again.
    gate.notify_one();
    let settled = frame_until(&mut usage, |frame| {
        frame["occupancy"]["contextWindow"].as_u64() == Some(selected)
    })
    .await;
    assert_eq!(
        settled["occupancy"]["contextWindow"],
        json!(selected),
        "{settled}"
    );
}

#[tokio::test]
async fn a_custom_model_has_no_denominator() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    // A bare custom model arrives through a legacy provider-settings.json.
    std::fs::write(
        fixture.data_dir.path().join("provider-settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "customModels": { "openai": ["gpt-private-2026-09-01"] }
        }))
        .unwrap(),
    )
    .unwrap();
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let custom = "openai/gpt-private-2026-09-01";
    select_model(&engine, custom).await;

    let mut usage = usage_watch(&engine).await;
    let frame = next_frame(&mut usage).await;
    assert_eq!(frame["occupancy"]["contextWindow"], json!(null), "{frame}");
}

#[tokio::test]
async fn the_record_count_is_replay_state_not_a_ledger_read() {
    let fixture = Fixture::new();
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::text("one"), ScriptedReply::text("two")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    for prompt in ["first prompt", "second prompt"] {
        run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
        wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    }
    let mut usage = usage_watch(&engine).await;
    let frame = frame_until(&mut usage, |frame| frame["recordCount"] == json!(2)).await;
    assert_eq!(frame["recordCount"], json!(2), "{frame}");

    // The ledger file goes away: the count is the runtime's own replay
    // state, so a fresh subscription still reports both records rather than
    // re-reading an absent file as zero.
    let ledger = fixture.data_dir.path().join("usage").join("chat-1.jsonl");
    std::fs::rename(&ledger, ledger.with_extension("jsonl.moved")).unwrap();
    let mut reopened = usage_watch(&engine).await;
    let after = next_frame(&mut reopened).await;
    assert_eq!(after["recordCount"], json!(2), "{after}");
    assert_eq!(after["gross"], json!(2 * 122), "{after}");
}

/// The preview targets the request the queue runs NEXT — the same answer as
/// the occupancy denominator — so a queued run on another model or folder is
/// what the report shows, not the composer's current selection.
#[tokio::test]
async fn preview_targets_the_request_the_queue_runs_next() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    select_model(&engine, "openai/gpt-5.4").await;
    let mut usage = usage_watch(&engine).await;

    // Pause the queue the way an interrupt does, so the enqueued run stays
    // pending: never admitted, so the chat row still carries the selection
    // above while the queue's head carries a different model and folder.
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    let queued_cwd = fixture.data_dir.path().join("queued-cwd");
    std::fs::create_dir(&queued_cwd).unwrap();
    std::fs::write(queued_cwd.join("AGENTS.md"), "QUEUED CWD RULES").unwrap();
    let queued_cwd = queued_cwd.to_string_lossy().into_owned();
    queue_run_on(&engine, &queued_cwd, "m-1", "openai/gpt-5.4-pro", "go").await;

    let RpcReply::Value(preview) = engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("PreviewChatContext did not return a value");
    };
    assert_eq!(
        preview["model"],
        json!("gpt-5.4-pro"),
        "the preview follows the queued head, not the chat's selection: {preview}"
    );
    let entries = preview["entries"].as_array().unwrap();
    let system = entries
        .iter()
        .find(|entry| entry["kind"] == json!("systemPrompt"))
        .expect("the preview carries the system prompt");
    // The AGENTS.md walk ran against the QUEUED request's cwd.
    let content = system["content"].as_str().unwrap();
    assert!(
        content.contains("QUEUED CWD RULES"),
        "the queued request's folder owns the preview's rules walk: {content}"
    );

    // The queue really held the item pending (no run booked anything), so
    // the head — not an admitted, config-stamped row — answered the preview.
    let frame = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
    })
    .await;
    assert_eq!(frame["recordCount"], json!(0), "{frame}");
}

/// Moving the model picker invalidates a published preview: the frame drops
/// its context until the next report open rebuilds against the new target.
#[tokio::test]
async fn moving_the_selection_rebuilds_a_published_preview() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    select_model(&engine, "openai/gpt-5.4").await;
    let mut usage = usage_watch(&engine).await;

    engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap();
    let preview = frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
    })
    .await;
    assert_eq!(preview["context"]["model"], json!("gpt-5.4"));

    select_model(&engine, "openai/gpt-5.4-pro").await;
    let cleared = frame_until(&mut usage, |frame| {
        // Skip the preview frame itself; the next frame is the clear's.
        frame["context"].is_null() && frame["occupancy"]["contextWindow"].is_u64()
    })
    .await;
    assert!(cleared["context"].is_null(), "{cleared}");

    // Reopening rebuilds against the new selection.
    let RpcReply::Value(rebuilt) = engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("PreviewChatContext did not return a value");
    };
    assert_eq!(rebuilt["model"], json!("gpt-5.4-pro"), "{rebuilt}");
}

/// Concurrent previews (one per chat) share one subprocess per MCP server:
/// the per-server single-flight gate must hold while both connect at once.
#[tokio::test]
async fn concurrent_previews_spawn_each_mcp_server_once() {
    let fixture = Fixture::new();
    let spawn_log = fixture.data_dir.path().join("mcp-spawn-log");
    std::fs::write(
        fixture.data_dir.path().join("mcp.json"),
        serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                "only": {
                    "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture"),
                    "env": {
                        // A long tools/list keeps both previews inside the
                        // connect window at once — the race the gate owns.
                        "FIXTURE_STALL_LIST_MS": "800",
                        "FIXTURE_SPAWN_LOG": spawn_log,
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![]);
    let engine = Arc::new(fixture.engine(&provider));
    setup_chat(&engine, "chat-1").await;
    setup_chat(&engine, "chat-2").await;
    select_model_on(&engine, "chat-1", "openai/gpt-5.4").await;
    select_model_on(&engine, "chat-2", "openai/gpt-5.4").await;

    let preview = |chat_id: &'static str| {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": chat_id}))
                .await
        })
    };
    let (a, b) = tokio::join!(preview("chat-1"), preview("chat-2"));
    a.unwrap().unwrap();
    b.unwrap().unwrap();

    let spawns = std::fs::read_to_string(&spawn_log).unwrap();
    assert_eq!(
        spawns.lines().count(),
        1,
        "one server, one subprocess, however many concurrent callers: {spawns}"
    );
}

/// The capture costs a full prompt/message copy, so it is skipped while
/// nobody holds a usage watch — an unwatched chat's report builds its
/// preview on demand instead of capturing every request for nobody.
#[tokio::test]
async fn a_turn_run_without_a_usage_watch_captures_no_context() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]).with_usage(Usage {
        input: 700,
        output: 70,
        cache_read: 7,
        cache_write: 3,
        total_tokens: 780,
        ..Default::default()
    });
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    // No usage watch is open while the turn runs.
    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let mut usage = usage_watch(&engine).await;
    let frame = next_frame(&mut usage).await;
    assert!(frame["context"].is_null(), "{frame}");
    assert_eq!(frame["recordCount"], json!(1), "{frame}");

    // The report's own open still builds one on demand.
    let RpcReply::Value(preview) = engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("PreviewChatContext did not return a value");
    };
    assert_eq!(preview["preview"], json!(true), "{preview}");
}

/// A preview published in a watched moment does not survive the unwatched
/// turn that supersedes it: the capture is skipped (nobody is watching),
/// and the preview it would have replaced is retired — the next
/// subscription's frame reports the slot empty, so the report rebuilds on
/// demand instead of showing the stale shape.
#[tokio::test]
async fn an_unwatched_turn_retires_a_stale_preview() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("done")]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    select_model(&engine, "openai/gpt-5.4").await;

    // The watched moment: the report opens and publishes a preview.
    let mut usage = usage_watch(&engine).await;
    engine
        .handle(methods::PREVIEW_CHAT_CONTEXT, json!({"chatId": "chat-1"}))
        .await
        .unwrap();
    frame_until(&mut usage, |frame| {
        frame["context"]["preview"] == json!(true)
    })
    .await;
    drop(usage);

    // The turn runs unwatched: its capture is skipped, and the preview it
    // superseded must not survive it.
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // Back on the chat: the opening frame reports the slot empty.
    let mut reopened = usage_watch(&engine).await;
    let frame = next_frame(&mut reopened).await;
    assert!(frame["context"].is_null(), "{frame}");
}

/// A captured real request is stale after a later unwatched request too:
/// keeping it would pair the old request's entries with the new occupancy.
#[tokio::test]
async fn an_unwatched_turn_retires_a_stale_actual_snapshot() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("first answer"),
        ScriptedReply::text("second answer"),
    ]);
    let engine = fixture.engine(&provider);
    setup_chat(&engine, "chat-1").await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    let mut usage = usage_watch(&engine).await;
    run_prompt(&engine, "chat-1", &fixture.cwd(), "first prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let actual = frame_until(&mut usage, |frame| {
        frame["recordCount"] == json!(1) && frame["context"]["preview"] == json!(false)
    })
    .await;
    assert!(
        actual["context"]["entries"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["content"] == json!("first prompt"))),
        "{actual}"
    );
    drop(usage);

    run_prompt(&engine, "chat-1", &fixture.cwd(), "second prompt").await;
    wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let mut reopened = usage_watch(&engine).await;
    let frame = next_frame(&mut reopened).await;
    assert!(frame["context"].is_null(), "{frame}");
}
