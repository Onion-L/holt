//! Minimal no-Node usage of the pi-agent-core port: drives a scripted
//! agent turn through the faux provider, records the transcript into a
//! JSONL session, and prints the persisted bytes and lifecycle events.

use std::sync::Arc;

use pi_core::agent::agent::{Agent, AgentOptions};
use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::session::jsonl::repo::JsonlSessionRepo;
use pi_core::agent::harness::session::jsonl::types::JsonlSessionCreateOptions;
use pi_core::agent::harness::session::jsonl::types::JsonlSessionRepoOptions;
use pi_core::agent::harness::session::memory::Session;
use pi_core::agent::harness::types::FileSystem;
use pi_core::ai::compat::register_faux_provider;
use pi_core::ai::providers::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
    faux_text, faux_tool_call,
};

#[tokio::main]
async fn main() {
    // 1. A scripted provider so the example runs without credentials.
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        token_size: Some(pi_core::ai::providers::faux::FauxTokenSize {
            min: Some(1),
            max: Some(3),
        }),
        ..Default::default()
    });
    faux.set_responses(vec![
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            vec![
                faux_text("Let me check."),
                faux_tool_call(
                    "bash",
                    serde_json::json!({ "command": "echo hi" }),
                    Some("call-1".to_string()),
                ),
            ],
            FauxMessageOptions {
                stop_reason: Some(pi_core::ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        ))),
        FauxResponseStep::Message(Box::new(faux_assistant_message(
            "hi",
            FauxMessageOptions::default(),
        ))),
    ]);

    // 2. An agent over the compat stream function.
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(Arc::new(|model, context, options| {
            Ok(pi_core::ai::compat::stream_simple(model, context, options))
        })),
        ..Default::default()
    });
    agent.set_model(faux.get_model());
    let recorded: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
    let listener_recorded = Arc::clone(&recorded);
    let _subscription = agent.subscribe(Arc::new(move |event, _signal| {
        let name = match event {
            pi_core::agent::types::AgentEvent::AgentStart => "agent_start",
            pi_core::agent::types::AgentEvent::AgentEnd { .. } => "agent_end",
            pi_core::agent::types::AgentEvent::TurnStart => "turn_start",
            pi_core::agent::types::AgentEvent::TurnEnd { .. } => "turn_end",
            pi_core::agent::types::AgentEvent::MessageStart { .. } => "message_start",
            pi_core::agent::types::AgentEvent::MessageUpdate { .. } => "message_update",
            pi_core::agent::types::AgentEvent::MessageEnd { .. } => "message_end",
            pi_core::agent::types::AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            pi_core::agent::types::AgentEvent::ToolExecutionUpdate { .. } => {
                "tool_execution_update"
            }
            pi_core::agent::types::AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        };
        listener_recorded.lock().unwrap().push(name);
        Box::pin(async {})
    }));
    agent.prompt("Run the example").await.expect("agent prompt");
    let event_names: Vec<&'static str> = recorded.lock().unwrap().clone();

    println!("== agent events ==");
    for name in &event_names {
        println!("  {name}");
    }

    // 3. Persist the transcript into a JSONL session through the Node-free
    //    execution environment.
    let root = std::env::temp_dir().join(format!("pi-example-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create example root");
    let env: Arc<NodeExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string_lossy().into_owned(),
        ..Default::default()
    }));
    let repo = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::clone(&env) as Arc<dyn FileSystem>,
        sessions_root: format!("{}/sessions", root.to_string_lossy()),
    });
    let session: Session = repo
        .create(JsonlSessionCreateOptions {
            id: Some("example-session".to_string()),
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .expect("create session");
    for message in agent.messages() {
        session.append_message(message).await.expect("append");
    }
    session
        .set_name(Some("Example session".to_string()))
        .await
        .unwrap();

    println!("== session stats ==");
    let stats = session.get_stats().await;
    println!("  messages: {}", stats.message_count);
    println!("  lanes: {}", session.get_lanes().await.len());
    for metadata in repo.list(&Default::default()).await.expect("list sessions") {
        println!("  session file: {}", metadata.path);
        let bytes = env
            .read_text_file(&metadata.path, None)
            .await
            .expect("read");
        print!("{bytes}");
    }

    let _ = env
        .remove(
            &format!("{}/sessions", root.to_string_lossy()),
            pi_core::agent::harness::types::RemoveOptions {
                recursive: Some(true),
                force: Some(true),
            },
            None,
        )
        .await;
    let _ = std::fs::remove_dir_all(&root);
}
