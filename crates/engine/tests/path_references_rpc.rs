//! Path references are plain prompt text end-to-end: the composer appends
//! a `Referenced paths:` list to ordinary sends and inlines quoted paths
//! into a skill's extra instructions, and the engine must carry that text
//! verbatim — into the queue, across a restart, through edits, and into
//! the model-visible prompt. No `holt-file:` URLs, no upload artifacts.

mod common;

use std::sync::Arc;

use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;

// -- helpers (mirroring message_queue_rpc.rs / queue_commands_rpc.rs) ----------

/// The shape the UI appends to an ordinary send that carries references.
const WITH_REFERENCES: &str =
    "look at these\n\nReferenced paths:\n- \"/abs/a.rs\"\n- \"/abs/dir/\"";

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

async fn wait_drained(engine: &holt_engine::LocalEngine) {
    wait_for_queue(engine, |q| {
        q["pending"] == json!([]) && q["activeMessageId"].is_null()
    })
    .await;
}

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

async fn edit_message(engine: &holt_engine::LocalEngine, message_id: &str, prompt: &str) {
    engine
        .handle(
            methods::EDIT_QUEUED_MESSAGE,
            json!({"chatId":"chat-1","messageId":message_id,"prompt":prompt}),
        )
        .await
        .unwrap();
}

/// The text of the last user message the model received in this request.
fn user_text(request: &common::RecordedRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_core::ai::types::Message::User(message) => Some(message.content.text().to_string()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

fn skill(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: the {name} skill.\n---\n{body}\n"),
    )
    .unwrap();
}

/// Queue an `invokeSkill` command exactly as the composer serializes it:
/// the path references ride inside `extraInstructions`, the request prompt
/// stays empty.
async fn invoke_skill(
    engine: &holt_engine::LocalEngine,
    cwd: &str,
    name: &str,
    extra: &str,
    message_id: &str,
) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": {
                    "kind": "invokeSkill",
                    "name": name,
                    "extraInstructions": extra,
                    "messageId": message_id,
                    "request": {
                        "prompt": "",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": "high",
                        "cwd": cwd,
                    },
                },
            }),
        )
        .await
        .unwrap();
}

mod path_references {
    use super::*;

    #[tokio::test]
    async fn an_ordinary_send_carries_the_path_list_verbatim() {
        let fixture = Fixture::new();
        let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        queue_run(&engine, &fixture.cwd(), "m-refs", WITH_REFERENCES).await;
        common::wait_for_requests(&provider, 1).await;

        // The model receives the exact text — readable absolute paths, no
        // holt-file: URLs, nothing uploaded or rewritten.
        assert_eq!(user_text(&provider.requests()[0]), WITH_REFERENCES);
        let raw = serde_json::to_string(&provider.requests()[0].messages).unwrap();
        assert!(!raw.contains("holt-file:"), "{raw}");

        // The transcript user entry carries the same text.
        let transcript = common::transcript_snapshot(&engine, "chat-1").await;
        let entry = transcript["reset"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["role"] == "user")
            .expect("a user entry");
        assert_eq!(entry["parts"][0]["text"], WITH_REFERENCES);
    }

    #[tokio::test]
    async fn a_references_only_send_needs_no_task_instruction() {
        let fixture = Fixture::new();
        let provider = ScriptedProvider::new(vec![ScriptedReply::text("answer")]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        let prompt = "Referenced paths:\n- \"/abs/a.rs\"";
        queue_run(&engine, &fixture.cwd(), "m-only", prompt).await;
        common::wait_for_requests(&provider, 1).await;

        // Nonempty validation accepted the header + list, and the model
        // gets exactly that — no task instruction is invented.
        assert_eq!(user_text(&provider.requests()[0]), prompt);
    }

    #[tokio::test]
    async fn a_skill_invocation_rides_inline_and_listed_paths_verbatim() {
        let fixture = Fixture::new();
        skill(fixture.personal_dir.path(), "grill", "GRILL-BODY");
        let gate = Arc::new(tokio::sync::Notify::new());
        let provider = ScriptedProvider::new(vec![
            ScriptedReply::gated(gate.clone(), "answer A"),
            ScriptedReply::text("skill answer"),
        ]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
        common::wait_for_requests(&provider, 1).await;

        let extra = "refactor \"/abs/a.rs\"\n\nReferenced paths:\n- \"/abs/dir/\"";
        invoke_skill(&engine, &fixture.cwd(), "grill", extra, "m-skill").await;

        // Pending: the extra instructions carry the paths, the request
        // prompt itself rides empty.
        let state = queue_state(&engine).await;
        assert_eq!(state["pending"][0]["kind"], "skill");
        assert_eq!(state["pending"][0]["extraInstructions"], extra);
        assert_eq!(state["pending"][0]["request"]["prompt"], "");

        gate.notify_one();
        wait_drained(&engine).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        let skill_prompt = user_text(&requests[1]);
        assert!(
            skill_prompt.contains("<skill name=\"grill\""),
            "{skill_prompt}"
        );
        assert!(skill_prompt.contains("GRILL-BODY"), "{skill_prompt}");
        assert!(skill_prompt.contains("\"/abs/a.rs\""), "{skill_prompt}");
        assert!(skill_prompt.contains("\"/abs/dir/\""), "{skill_prompt}");
        assert!(skill_prompt.ends_with(extra), "{skill_prompt}");
    }

    #[tokio::test]
    async fn an_accepted_path_list_survives_restart_and_edits() {
        let fixture = Fixture::new();
        let provider =
            ScriptedProvider::new(vec![ScriptedReply::Silent, ScriptedReply::text("reply")]);
        let engine = fixture.engine(&provider);
        common::setup_chat(&engine, "chat-1").await;
        common::run_prompt(&engine, "chat-1", &fixture.cwd(), "A").await;
        common::wait_for_requests(&provider, 1).await;
        queue_run(&engine, &fixture.cwd(), "m-refs", WITH_REFERENCES).await;
        engine
            .handle(
                methods::QUEUE_COMMAND,
                json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
            )
            .await
            .unwrap();
        wait_for_queue(&engine, |q| q["paused"] == true).await;

        // Edit the path text before execution, then restart the engine.
        let edited = "look at these\n\nReferenced paths:\n- \"/abs/other.rs\"";
        edit_message(&engine, "m-refs", edited).await;
        drop(engine);

        let engine = fixture.engine(&provider);
        let state = queue_state(&engine).await;
        assert_eq!(state["paused"], true);
        assert_eq!(state["pending"][0]["messageId"], "m-refs");
        assert_eq!(state["pending"][0]["request"]["prompt"], edited);
        assert_eq!(provider.requests().len(), 1, "restart must not execute");

        // Unpausing runs the edited text — that is what the model sees.
        engine
            .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
            .await
            .unwrap();
        wait_drained(&engine).await;
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(user_text(&provider.requests()[1]), edited);
    }
}
