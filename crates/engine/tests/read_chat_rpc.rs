mod common;

use common::{ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcService, methods};

#[tokio::test]
async fn read_chat_returns_visible_text_and_records_a_named_chip() {
    let fixture = common::Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::text("target answer"),
        ScriptedReply::tool_call(
            "read-chat-1",
            "read_chat",
            serde_json::json!({ "url": "placeholder" }),
        ),
        ScriptedReply::text("caller conclusion"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "target").await;
    common::setup_chat(&engine, "caller").await;
    let (_, mut target_sessions) = common::subscribe(&engine, "target").await;
    common::run_prompt(&engine, "target", &fixture.cwd(), "target question").await;
    common::wait_for_session_status(&mut target_sessions, "target", "idle").await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "renameChat",
                "chatId": "target",
                "title": "Target Chat",
            }),
        )
        .await
        .unwrap();

    let workspace = holt_proto::workspace_locator(
        Some(engine.engine_info().workspace_scope),
        None,
        Some(&engine.engine_info().device_id),
    )
    .unwrap();
    let url = holt_proto::holt_chat_link("target", &workspace);
    provider.replace_tool_arguments("read-chat-1", serde_json::json!({ "url": url }));

    let (_, mut caller_sessions) = common::subscribe(&engine, "caller").await;
    common::run_prompt(&engine, "caller", &fixture.cwd(), "inspect the Chat link").await;
    common::wait_for_session_status(&mut caller_sessions, "caller", "idle").await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tool_names.contains(&"read_chat".into()));
    assert!(
        requests[1]
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("call `read_chat` immediately")
    );
    let result_round = common::summarize(&requests[2].messages).join("\n");
    assert!(result_round.contains("target question"));
    assert!(result_round.contains("target answer"));
    assert!(result_round.contains("untrusted Chat data"));

    let snapshot = common::transcript_snapshot(&engine, "caller").await;
    let chip = snapshot["reset"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().unwrap())
        .find(|part| part["id"] == "read-chat-1")
        .unwrap();
    assert_eq!(chip["call"]["kind"], "readChat");
    assert_eq!(chip["call"]["chatId"], "target");
    assert_eq!(chip["call"]["title"], "Target Chat");
    assert_eq!(chip["resolved"], true);
    assert_eq!(chip["isError"], false);
}
