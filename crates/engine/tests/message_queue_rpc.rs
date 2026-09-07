mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;

async fn queue_state(engine: &holt_engine::LocalEngine) -> serde_json::Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    common::next_frame(&mut watch).await
}

async fn wait_for_queue(
    engine: &holt_engine::LocalEngine,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let frame = common::next_frame(&mut watch).await;
        if ready(&frame) {
            return frame;
        }
    }
}

#[tokio::test]
async fn ordinary_messages_wait_their_turn_without_entering_the_transcript() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("answer B"),
        ScriptedReply::text("answer C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "C").await;

    let RpcReply::Stream(mut queue) = engine
        .handle("WatchMessageQueue", json!({"chatId": "chat-1"}))
        .await
        .unwrap()
    else {
        panic!("expected queue watch")
    };
    let state = common::next_frame(&mut queue).await;
    assert_eq!(state["paused"], false);
    assert_eq!(state["pending"][0]["request"]["prompt"], "B");
    assert_eq!(state["pending"][1]["request"]["prompt"], "C");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!transcript.to_string().contains("\"text\":\"B\""));
    assert_eq!(provider.requests().len(), 1);

    gate.notify_one();
    loop {
        let state = common::next_frame(&mut queue).await;
        if state["pending"] == json!([]) && state["activeMessageId"].is_null() {
            break;
        }
    }
    assert_eq!(provider.requests().len(), 3);
    let prompts: Vec<_> = provider
        .requests()
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .filter_map(|message| match message {
                    pi_core::ai::types::Message::User(message) => {
                        Some(serde_json::to_value(&message.content).unwrap())
                    }
                    _ => None,
                })
                .last()
                .unwrap()
        })
        .collect();
    assert_eq!(prompts, vec![json!("A"), json!("B"), json!("C")]);
}

#[tokio::test]
async fn continue_during_cancellation_waits_for_cleanup_then_runs_the_queue() {
    let fixture = Fixture::new();
    let observed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    assert_eq!(
        provider.requests().len(),
        1,
        "Continue must wait for the transport to finish"
    );
    finish.notify_one();
    common::wait_for_requests(&provider, 2).await;
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(transcript.to_string().contains("answer B"));
    assert!(transcript.to_string().contains("aborted"));
}

#[cfg(unix)]
#[tokio::test]
async fn continue_after_shell_interrupt_waits_for_descendant_cleanup() {
    use std::time::Duration;

    struct ChildGuard(i32);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            // SAFETY: this PID belongs to the test's writing child.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "shell-a",
            "bash",
            json!({"command": "bash -c 'trap \"\" TERM; echo $$ > child.pid; for ((i=0; i<500; i++)); do echo tick >> writes; sleep 0.01; done' & wait"}),
        ),
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    let writes = fixture.project_dir.path().join("writes");
    let pid_file = fixture.project_dir.path().join("child.pid");
    let _child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = pid.trim().parse::<i32>()
                && writes.exists()
            {
                break ChildGuard(pid);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bash tool never started its child");

    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;

    let stopped = std::fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(std::fs::read(&writes).unwrap(), stopped);
    assert_eq!(provider.requests().len(), 2);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(transcript.to_string().contains("answer B"));
    assert!(transcript.to_string().contains("aborted"));
}

#[tokio::test]
async fn restart_retains_pending_messages_paused_and_never_replays_a_started_turn() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Silent,
        ScriptedReply::text("answer B"),
        ScriptedReply::text("answer C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    assert_eq!(
        queue_state(&engine).await["pending"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(engine);

    let engine = fixture.engine(&provider);
    let state = queue_state(&engine).await;
    assert_eq!(state["paused"], true);
    assert_eq!(state["activeMessageId"], serde_json::Value::Null);
    assert_eq!(state["pending"][0]["request"]["prompt"], "B");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(transcript.to_string().contains("aborted"));
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "C").await;
    let state = queue_state(&engine).await;
    assert_eq!(state["paused"], true);
    assert_eq!(state["pending"].as_array().unwrap().len(), 2);
    assert_eq!(provider.requests().len(), 1);
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 3);
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    assert_eq!(queue_state(&engine).await["pending"], json!([]));
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn an_admission_error_retains_the_head_until_credentials_are_restored() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer A")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::REMOVE_PROVIDER_KEY, json!({"providerId":"openai"}))
        .await
        .unwrap();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    let state = wait_for_queue(&engine, |q| q["paused"] == true).await;
    assert!(
        state["pending"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not configured")
    );
    assert_eq!(state["pending"][0]["request"]["prompt"], "A");
    assert_eq!(
        common::transcript_snapshot(&engine, "chat-1").await["reset"],
        json!([])
    );
    assert!(provider.requests().is_empty());
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            json!({"providerId":"openai","key":"restored-test-key"}),
        )
        .await
        .unwrap();
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["activeMessageId"].is_null() && q["pending"] == json!([])
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn a_failed_enqueue_is_not_acknowledged_or_remembered_as_a_duplicate() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer A")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    // The filesystem boundary rejects creation of the queue directory.
    std::fs::write(fixture.data_dir.path().join("queues"), "blocked").unwrap();
    let command = json!({"chatId":"chat-1","command":{"kind":"run","messageId":"m-a","request":{
        "prompt":"A","provider":"openai","model":"openai/gpt-5.4","cwd":fixture.cwd(),"reasoning":null
    }}});
    assert!(
        engine
            .handle(methods::QUEUE_COMMAND, command.clone())
            .await
            .is_err()
    );
    assert!(provider.requests().is_empty());
    std::fs::remove_file(fixture.data_dir.path().join("queues")).unwrap();
    engine
        .handle(methods::QUEUE_COMMAND, command)
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["activeMessageId"].is_null() && q["pending"] == json!([])
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn execution_failure_keeps_the_failed_turn_and_pauses_the_rest() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "first"),
        ScriptedReply::Failed("provider offline".into()),
        ScriptedReply::text("answer C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    for prompt in ["A", "B", "C"] {
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
    }
    gate.notify_one();
    let state = wait_for_queue(&engine, |q| {
        q["paused"] == true && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(state["pending"].as_array().unwrap().len(), 1);
    assert_eq!(state["pending"][0]["request"]["prompt"], "C");
    assert_eq!(provider.requests().len(), 2);
    assert!(
        common::transcript_snapshot(&engine, "chat-1")
            .await
            .to_string()
            .contains("provider offline")
    );
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn stop_during_approval_keeps_pending_messages_paused_and_never_grants_the_tool() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "write-a",
            "write",
            json!({"path":"blocked.txt","content":"no"}),
        ),
        ScriptedReply::text("answer B"),
        ScriptedReply::text("answer C"),
        ScriptedReply::text("answer D"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_gate(&engine, "chat-1", "write-a", "pending").await;
    for prompt in ["B", "C"] {
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), prompt).await;
    }
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["paused"] == true && q["activeMessageId"].is_null()
    })
    .await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "D").await;
    assert_eq!(
        queue_state(&engine).await["pending"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(provider.requests().len(), 1);
    assert!(!fixture.project_dir.path().join("blocked.txt").exists());
    assert!(
        common::transcript_snapshot(&engine, "chat-1")
            .await
            .to_string()
            .contains("aborted")
    );
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 4);
}

#[tokio::test]
async fn pending_model_and_reasoning_are_fixed_but_permission_is_read_at_start() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::tool_call(
            "write-b",
            "write",
            json!({"path":"allowed.txt","content":"yes"}),
        ),
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    engine.handle(methods::QUEUE_COMMAND, json!({"chatId":"chat-1","command":{"kind":"run","messageId":"b","request":{
        "prompt":"B","provider":"openai","model":"openai/gpt-5.4","reasoning":"high","cwd":fixture.cwd()
    }}})).await.unwrap();
    engine.handle(methods::MUTATE, json!({"op":"setChatConfig","chatId":"chat-1","config":{"provider":"openai","model":"openai/gpt-5.4-mini","reasoning":"low"}})).await.unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({"op":"setChatPermissionMode","chatId":"chat-1","mode":"full-access"}),
        )
        .await
        .unwrap();
    gate.notify_one();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests()[1].model, "gpt-5.4");
    assert_eq!(provider.requests()[1].reasoning.as_deref(), Some("High"));
    assert_eq!(
        std::fs::read_to_string(fixture.project_dir.path().join("allowed.txt")).unwrap(),
        "yes"
    );
}

#[tokio::test]
async fn a_started_checkpoint_recovers_even_before_the_first_transcript_or_history_write() {
    let fixture = Fixture::new();
    let provider =
        ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("answer B")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    drop(engine);
    // Reproduce a crash immediately after the durable admission checkpoint.
    std::fs::remove_file(fixture.data_dir.path().join("transcripts/chat-1.json")).unwrap();
    std::fs::remove_file(fixture.data_dir.path().join("history/chat-1.jsonl")).unwrap();
    let engine = fixture.engine(&provider);
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        transcript
            .to_string()
            .contains("Turn interrupted by restart")
    );
    assert_eq!(
        transcript["reset"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["role"] == "user")
            .count(),
        1
    );
    assert_eq!(
        queue_state(&engine).await["pending"][0]["request"]["prompt"],
        "B"
    );
    assert_eq!(provider.requests().len(), 1);
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(
        common::summarize(&provider.requests()[1].messages),
        ["user:A", "user:B"]
    );
}

#[tokio::test]
async fn stop_propagates_through_all_compaction_placements_and_pauses_pending_work() {
    for placement in ["before", "between", "manual"] {
        let fixture = Fixture::new();
        let usage = pi_core::ai::types::Usage {
            input: 260_000,
            output: 500,
            total_tokens: 260_500,
            ..common::fixed_usage()
        };
        let first = if placement == "between" {
            ScriptedReply::tool_call_with_usage(
                "read-a",
                "read",
                json!({"path":"notes.txt"}),
                usage,
            )
        } else {
            ScriptedReply::text_with_usage(
                "conversation ".repeat(10_000),
                if placement == "manual" {
                    common::fixed_usage()
                } else {
                    usage
                },
            )
        };
        std::fs::write(fixture.project_dir.path().join("notes.txt"), "notes").unwrap();
        let provider = ScriptedProvider::new(vec![first, ScriptedReply::Silent]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
        if placement != "between" {
            wait_for_queue(&engine, |q| {
                q["pending"] == json!([]) && q["activeMessageId"].is_null()
            })
            .await;
            if placement == "before" {
                common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
            } else {
                engine.handle(methods::QUEUE_COMMAND, json!({"chatId":"chat-1","command":{"kind":"compact","request":{
                    "prompt":"","provider":"openai","model":"openai/gpt-5.4","cwd":fixture.cwd()
                }}})).await.unwrap();
            }
        }
        common::wait_for_requests(&provider, 2).await;
        assert_eq!(provider.requests()[1].tools, 0, "{placement}");
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "pending").await;
        let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;
        engine
            .handle(
                methods::QUEUE_COMMAND,
                json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
            )
            .await
            .unwrap();
        common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
        let queue = queue_state(&engine).await;
        assert_eq!(queue["paused"], true, "{placement}");
        assert_eq!(
            queue["pending"][0]["request"]["prompt"], "pending",
            "{placement}"
        );
        assert_eq!(provider.requests().len(), 2, "{placement}");
        assert!(
            !common::transcript_snapshot(&engine, "chat-1")
                .await
                .to_string()
                .contains("Turn continues")
        );
    }
}

#[tokio::test]
async fn stop_still_pauses_when_its_save_fails_and_storage_recovers_before_cleanup() {
    let fixture = Fixture::new();
    let observed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Cancelling {
            observed: observed.clone(),
            finish: finish.clone(),
        },
        ScriptedReply::text("answer B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "B").await;
    let queues = fixture.data_dir.path().join("queues");
    let saved = fixture.data_dir.path().join("queues-saved");
    std::fs::rename(&queues, &saved).unwrap();
    std::fs::write(&queues, "blocked").unwrap();
    assert!(
        engine
            .handle(
                methods::QUEUE_COMMAND,
                json!({"chatId":"chat-1","command":{"kind":"interrupt"}})
            )
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    assert_eq!(queue_state(&engine).await["paused"], true);
    std::fs::remove_file(&queues).unwrap();
    std::fs::rename(&saved, &queues).unwrap();
    finish.notify_one();
    wait_for_queue(&engine, |q| q["activeMessageId"].is_null()).await;
    assert_eq!(queue_state(&engine).await["paused"], true);
    assert_eq!(provider.requests().len(), 1);
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 2);
}

// -- Ticket 02: editing and deleting pending messages ------------------------

async fn mutate_call(
    engine: &holt_engine::LocalEngine,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, holt_rpc::RpcError> {
    match engine.handle(method, params).await? {
        RpcReply::Value(value) => Ok(value),
        _ => panic!("expected a unary queue mutation reply"),
    }
}

async fn edit_message(
    engine: &holt_engine::LocalEngine,
    message_id: &str,
    prompt: &str,
) -> Result<serde_json::Value, holt_rpc::RpcError> {
    mutate_call(
        engine,
        methods::EDIT_QUEUED_MESSAGE,
        json!({"chatId":"chat-1","messageId":message_id,"prompt":prompt}),
    )
    .await
}

async fn delete_message(
    engine: &holt_engine::LocalEngine,
    message_id: &str,
) -> Result<serde_json::Value, holt_rpc::RpcError> {
    mutate_call(
        engine,
        methods::DELETE_QUEUED_MESSAGE,
        json!({"chatId":"chat-1","messageId":message_id}),
    )
    .await
}

/// Queue a run command with an explicit identity, as the composer does.
async fn queue_run(engine: &holt_engine::LocalEngine, cwd: &str, message_id: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"run","messageId":message_id,"request":{
                "prompt":prompt,"provider":"openai","model":"openai/gpt-5.4",
                "reasoning":"high","cwd":cwd
            }}}),
        )
        .await
        .unwrap();
}

fn user_text(request: &common::RecordedRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_core::ai::types::Message::User(message) => {
                Some(format!("{}", message.content.text()))
            }
            _ => None,
        })
        .last()
        .unwrap_or_default()
}

#[tokio::test]
async fn an_edit_changes_only_the_body_and_the_turn_sends_it_exactly_once() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("reply B"),
        ScriptedReply::text("reply C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B original").await;
    queue_run(&engine, &fixture.cwd(), "m-c", "C").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    // The gated transport does not observe cancellation on its own; release
    // it so A ends and the pause settles.
    gate.notify_one();
    wait_for_queue(&engine, |q| {
        q["paused"] == true && q["activeMessageId"].is_null()
    })
    .await;

    let snapshot = edit_message(&engine, "m-b", "B edited").await.unwrap();
    assert_eq!(snapshot["pending"][0]["messageId"], "m-b");
    assert_eq!(snapshot["pending"][0]["request"]["prompt"], "B edited");
    assert_eq!(snapshot["pending"][0]["request"]["model"], "openai/gpt-5.4");
    assert_eq!(snapshot["pending"][0]["request"]["reasoning"], "high");
    assert_eq!(snapshot["pending"][1]["request"]["prompt"], "C");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(!transcript.to_string().contains("B edited"));

    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 3);
    let b_request = &provider.requests()[1];
    assert_eq!(user_text(b_request), "B edited");
    assert_eq!(b_request.model, "gpt-5.4");
    assert_eq!(b_request.reasoning.as_deref(), Some("High"));
    assert_eq!(user_text(&provider.requests()[2]), "C");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert_eq!(transcript.to_string().matches("B edited").count(), 1);
    assert!(!transcript.to_string().contains("B original"));
}

#[tokio::test]
async fn deleting_a_pending_item_never_creates_a_transcript_or_history_message() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("reply C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "deleted-B").await;
    queue_run(&engine, &fixture.cwd(), "m-c", "kept-C").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    // Release the gated transport so A ends and the pause settles.
    gate.notify_one();
    wait_for_queue(&engine, |q| q["paused"] == true).await;

    let snapshot = delete_message(&engine, "m-b").await.unwrap();
    assert_eq!(
        snapshot["pending"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["request"]["prompt"].clone())
            .collect::<Vec<_>>(),
        vec![json!("kept-C")]
    );
    assert!(
        !common::transcript_snapshot(&engine, "chat-1")
            .await
            .to_string()
            .contains("deleted-B")
    );

    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(user_text(&provider.requests()[1]), "kept-C");
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(!transcript.contains("deleted-B"));
    assert!(transcript.contains("kept-C"));
}

#[tokio::test]
async fn deleting_the_last_failed_item_allows_the_next_send_after_restart() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("hello")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::REMOVE_PROVIDER_KEY, json!({"providerId":"openai"}))
        .await
        .unwrap();
    queue_run(&engine, &fixture.cwd(), "m-failed", "discard me").await;
    wait_for_queue(&engine, |q| q["paused"] == true).await;

    let snapshot = delete_message(&engine, "m-failed").await.unwrap();
    assert_eq!(snapshot["pending"], json!([]));
    assert_eq!(snapshot["paused"], false);
    assert!(snapshot["activeMessageId"].is_null());
    assert!(snapshot["error"].is_null());
    drop(engine);

    let engine = fixture.engine(&provider);
    assert_eq!(queue_state(&engine).await["paused"], false);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            json!({"providerId":"openai","key":"restored-test-key"}),
        )
        .await
        .unwrap();
    queue_run(&engine, &fixture.cwd(), "m-hi", "hi").await;
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(user_text(&provider.requests()[0]), "hi");
}

#[tokio::test]
async fn deleting_a_failed_head_lets_continue_admit_the_next_item() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer B")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::REMOVE_PROVIDER_KEY, json!({"providerId":"openai"}))
        .await
        .unwrap();
    queue_run(&engine, &fixture.cwd(), "m-a", "head-A").await;
    wait_for_queue(&engine, |q| q["paused"] == true).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "next-B").await;
    assert_eq!(
        queue_state(&engine).await["pending"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let snapshot = delete_message(&engine, "m-a").await.unwrap();
    assert_eq!(snapshot["pending"][0]["request"]["prompt"], "next-B");
    assert_eq!(
        snapshot["paused"], true,
        "deleting must not resume the queue"
    );
    assert_eq!(provider.requests().len(), 0);

    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            json!({"providerId":"openai","key":"restored-test-key"}),
        )
        .await
        .unwrap();
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(user_text(&provider.requests()[0]), "next-B");
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(!transcript.contains("head-A"));
}

#[tokio::test]
async fn a_failed_head_stays_editable_and_paused_until_continue() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer A")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::REMOVE_PROVIDER_KEY, json!({"providerId":"openai"}))
        .await
        .unwrap();
    queue_run(&engine, &fixture.cwd(), "m-a", "head-A").await;
    wait_for_queue(&engine, |q| q["paused"] == true).await;

    let snapshot = edit_message(&engine, "m-a", "head-A edited").await.unwrap();
    assert_eq!(
        snapshot["paused"], true,
        "editing must not resume the queue"
    );
    assert_eq!(snapshot["pending"][0]["request"]["prompt"], "head-A edited");
    assert!(
        snapshot["pending"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not configured"),
        "the admission error stays visible"
    );
    assert_eq!(provider.requests().len(), 0);

    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            json!({"providerId":"openai","key":"restored-test-key"}),
        )
        .await
        .unwrap();
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(user_text(&provider.requests()[0]), "head-A edited");
}

#[tokio::test]
async fn accepted_mutations_survive_a_restart_paused() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::Silent,
        ScriptedReply::text("reply B"),
        ScriptedReply::text("reply D"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B original").await;
    queue_run(&engine, &fixture.cwd(), "m-c", "gone-C").await;
    queue_run(&engine, &fixture.cwd(), "m-d", "kept-D").await;
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    wait_for_queue(&engine, |q| q["paused"] == true).await;
    edit_message(&engine, "m-b", "B edited").await.unwrap();
    delete_message(&engine, "m-c").await.unwrap();
    drop(engine);

    let engine = fixture.engine(&provider);
    let state = queue_state(&engine).await;
    assert_eq!(state["paused"], true);
    assert_eq!(state["activeMessageId"], serde_json::Value::Null);
    assert_eq!(state["pending"][0]["messageId"], "m-b");
    assert_eq!(state["pending"][0]["request"]["prompt"], "B edited");
    assert_eq!(state["pending"][0]["request"]["reasoning"], "high");
    assert_eq!(state["pending"][1]["request"]["prompt"], "kept-D");
    assert_eq!(provider.requests().len(), 1, "restart must not execute");

    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(user_text(&provider.requests()[1]), "B edited");
    assert_eq!(user_text(&provider.requests()[2]), "kept-D");
}

#[tokio::test]
async fn a_mutation_that_cannot_persist_is_not_acknowledged() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer B")]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::REMOVE_PROVIDER_KEY, json!({"providerId":"openai"}))
        .await
        .unwrap();
    queue_run(&engine, &fixture.cwd(), "m-a", "head-A").await;
    wait_for_queue(&engine, |q| q["paused"] == true).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "keep-B").await;

    let queues = fixture.data_dir.path().join("queues");
    let saved = fixture.data_dir.path().join("queues-saved");
    std::fs::rename(&queues, &saved).unwrap();
    std::fs::write(&queues, "blocked").unwrap();
    assert!(edit_message(&engine, "m-b", "edited-B").await.is_err());
    assert!(delete_message(&engine, "m-a").await.is_err());
    std::fs::remove_file(&queues).unwrap();
    std::fs::rename(&saved, &queues).unwrap();

    let state = queue_state(&engine).await;
    assert_eq!(state["pending"][1]["request"]["prompt"], "keep-B");
    assert_eq!(state["pending"].as_array().unwrap().len(), 2);

    let snapshot = edit_message(&engine, "m-b", "edited-B").await.unwrap();
    assert_eq!(snapshot["pending"][1]["request"]["prompt"], "edited-B");
}

#[tokio::test]
async fn a_started_message_refuses_edit_and_delete_and_runs_once() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("answer A"),
        ScriptedReply::gated(gate.clone(), "answer solo-B"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    queue_run(&engine, &fixture.cwd(), "m-b", "solo-B original").await;
    wait_for_queue(&engine, |q| q["activeMessageId"] == "m-b").await;

    let error = edit_message(&engine, "m-b", "solo-B edited")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already executing"), "{error}");
    let error = delete_message(&engine, "m-b").await.unwrap_err();
    assert!(error.to_string().contains("already executing"), "{error}");

    gate.notify_one();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(user_text(&provider.requests()[1]), "solo-B original");
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert_eq!(transcript.matches("solo-B original").count(), 1);
    assert!(!transcript.contains("solo-B edited"));
}

#[tokio::test]
async fn editing_while_a_turn_runs_wins_without_resuming_a_paused_queue() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("reply B"),
        ScriptedReply::text("reply C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "B original").await;
    queue_run(&engine, &fixture.cwd(), "m-c", "C").await;
    // The edit races A's whole remaining run: the queue is live (unpaused)
    // and B is waiting. It must land without starting anything and without
    // touching the running Turn.
    let snapshot = edit_message(&engine, "m-b", "B edited").await.unwrap();
    assert_eq!(snapshot["paused"], false);
    assert!(
        !snapshot["activeMessageId"].is_null(),
        "A must still be the active Turn"
    );
    assert_eq!(snapshot["pending"][0]["request"]["prompt"], "B edited");
    assert_eq!(provider.requests().len(), 1);

    gate.notify_one();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(user_text(&provider.requests()[1]), "B edited");
}

#[tokio::test]
async fn deleting_while_a_turn_runs_removes_the_item_from_execution() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "answer A"),
        ScriptedReply::text("reply C"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    queue_run(&engine, &fixture.cwd(), "m-b", "dropped-B").await;
    queue_run(&engine, &fixture.cwd(), "m-c", "kept-C").await;
    // The delete races A's whole remaining run on a live queue: it must land
    // durably, so B never reaches a provider even though the consumer was
    // free to pick it the moment A ended.
    let snapshot = delete_message(&engine, "m-b").await.unwrap();
    assert_eq!(snapshot["paused"], false);
    assert_eq!(
        snapshot["pending"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["request"]["prompt"].clone())
            .collect::<Vec<_>>(),
        vec![json!("kept-C")]
    );
    assert_eq!(provider.requests().len(), 1);

    gate.notify_one();
    wait_for_queue(&engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(user_text(&provider.requests()[1]), "kept-C");
    let transcript = common::transcript_snapshot(&engine, "chat-1")
        .await
        .to_string();
    assert!(!transcript.contains("dropped-B"));
}
