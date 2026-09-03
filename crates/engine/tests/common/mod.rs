//! Shared fixtures for the scripted-provider test seam: a fake model
//! transport injected through `EngineConfig::stream_fn` so RPC-handle tests
//! drive a full Turn without a real provider. The provider records the
//! message list of every request it receives and replies from a script —
//! text, tool calls, an aborted stream, a provider error, a hanging
//! stream — with fixed usage numbers so token-dependent behavior stays
//! deterministic.
//!
//! The `Fixture` and watch helpers around it assemble a real engine on
//! temp dirs and drive it through `RpcService::handle` exactly as the UI
//! does. Every later History/Compaction test rides this seam; no
//! production code path depends on it.

#![allow(dead_code)] // each test binary links the module whole

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use pi_core::agent::types::StreamFn;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, BlockContent, Context, DoneReason,
    ErrorReason, Message, Model, StopReason, TextContent, ToolCall, Usage,
};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// The scripted provider
// ---------------------------------------------------------------------------

/// One scripted model reply — what the "provider" answers the next request
/// it receives.
pub enum ScriptedReply {
    /// A finished text reply.
    Text(String),
    /// Tool calls: the loop executes them against the chat's working
    /// directory and asks the model again, so the script needs a following
    /// entry for the second round.
    ToolCalls(Vec<ToolCall>),
    /// A stream cut mid-text — the shape an interruption leaves behind: the
    /// partial content and any tool calls already streamed land, stop
    /// reason `aborted`, and the calls never execute.
    Aborted {
        partial: String,
        tool_calls: Vec<ToolCall>,
    },
    /// A provider failure: the error string rides the assistant message,
    /// stop reason `error`.
    Failed(String),
    /// A stream that never terminates — the "engine killed mid-Turn"
    /// stand-in: nothing arrives and the Turn hangs, so the test can drop
    /// the engine with the run in flight.
    Silent,
}

impl ScriptedReply {
    pub fn text(text: impl Into<String>) -> Self {
        ScriptedReply::Text(text.into())
    }

    /// A single tool call with the given id, name, and JSON arguments.
    pub fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Self {
        ScriptedReply::ToolCalls(vec![tool_call(id, name, arguments)])
    }

    /// An interruption with only partial text on the wire.
    pub fn aborted(partial: impl Into<String>) -> Self {
        ScriptedReply::Aborted {
            partial: partial.into(),
            tool_calls: Vec::new(),
        }
    }

    /// An interruption that cut the stream after tool calls had streamed
    /// but before they could run.
    pub fn aborted_with_tool_calls(partial: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        ScriptedReply::Aborted {
            partial: partial.into(),
            tool_calls,
        }
    }
}

/// A pi-core `ToolCall` with the fields the engine's tools read (the rest
/// default).
pub fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// A scripted provider: records the message list of every request, replies
/// from a queue of [`ScriptedReply`]s. The usage reported by every reply is
/// fixed at construction so compaction thresholds in tests are
/// deterministic.
pub struct ScriptedProvider {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<VecDeque<ScriptedReply>>>,
    usage: Usage,
}

/// The usage every scripted reply reports by default — small, fixed, and
/// distinct in each field so a leaked real usage would stand out.
pub fn fixed_usage() -> Usage {
    Usage {
        input: 111,
        output: 11,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 122,
        ..Default::default()
    }
}

impl ScriptedProvider {
    pub fn new(script: Vec<ScriptedReply>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(script.into())),
            usage: fixed_usage(),
        }
    }

    /// Pin the usage every reply reports (the knob compaction tests turn to
    /// cross the threshold deterministically).
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// The transport to inject through `EngineConfig::stream_fn`.
    pub fn stream_fn(&self) -> StreamFn {
        let requests = Arc::clone(&self.requests);
        let script = Arc::clone(&self.script);
        let usage = self.usage.clone();
        Arc::new(move |model: &Model, context: &Context, _options| {
            requests.lock().unwrap().push(RecordedRequest {
                messages: context.messages.clone(),
                system_prompt: context.system_prompt.clone(),
                tools: context.tools.as_ref().map_or(0, Vec::len),
            });
            let reply = script.lock().unwrap().pop_front().unwrap_or_else(|| {
                ScriptedReply::Failed("scripted provider ran out of replies".into())
            });
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            push_reply(&stream, reply, model, &usage);
            Ok(stream)
        })
    }

    /// Each request in arrival order — what the model would receive, plus
    /// the system prompt and tool count it was served with.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// One recorded provider request.
#[derive(Clone)]
pub struct RecordedRequest {
    pub messages: Vec<Message>,
    pub system_prompt: Option<String>,
    /// How many tools the request advertised (0 = a bare completion).
    pub tools: usize,
}

/// Render one scripted reply onto a fresh stream as its terminal event —
/// the same event shape the real transports terminate with, so the loop's
/// finalize path sees an authentic message.
fn push_reply(
    stream: &pi_core::ai::utils::event_stream::AssistantMessageEventStream,
    reply: ScriptedReply,
    model: &Model,
    usage: &Usage,
) {
    let mut message = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: usage.clone(),
        timestamp: now_millis(),
        ..Default::default()
    };
    match reply {
        ScriptedReply::Text(text) => {
            message.content = vec![AssistantContent::Text(TextContent {
                text,
                ..Default::default()
            })];
            message.stop_reason = StopReason::Stop;
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message,
            });
        }
        ScriptedReply::ToolCalls(calls) => {
            message.content = calls.into_iter().map(AssistantContent::ToolCall).collect();
            message.stop_reason = StopReason::ToolUse;
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::ToolUse,
                message,
            });
        }
        ScriptedReply::Aborted {
            partial,
            tool_calls,
        } => {
            // The half-streamed shape: a start with the partial content,
            // then the abort terminal carrying the same content — mirroring
            // the real transports' aborted message (stop reason `aborted`,
            // "Request was aborted" as its error message).
            let mut content = vec![AssistantContent::Text(TextContent {
                text: partial,
                ..Default::default()
            })];
            content.extend(tool_calls.into_iter().map(AssistantContent::ToolCall));
            message.content = content;
            message.stop_reason = StopReason::Aborted;
            message.error_message = Some("Request was aborted".into());
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error: message,
            });
        }
        ScriptedReply::Failed(error) => {
            message.stop_reason = StopReason::Error;
            message.error_message = Some(error);
            stream.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: message,
            });
        }
        // Nothing is pushed: `next` never resolves, the Turn never ends.
        ScriptedReply::Silent => {}
    }
}

// ---------------------------------------------------------------------------
// The engine fixture and RPC-driving helpers
// ---------------------------------------------------------------------------

pub struct Fixture {
    /// The chat's working directory.
    pub project_dir: TempDir,
    /// The personal skill root override — pinned empty so the system prompt
    /// stays fixture-driven, not machine-driven.
    pub personal_dir: TempDir,
    pub data_dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            project_dir: TempDir::new().unwrap(),
            personal_dir: TempDir::new().unwrap(),
            data_dir: TempDir::new().unwrap(),
        }
    }

    pub fn engine(&self, provider: &ScriptedProvider) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: Some(self.personal_dir.path().to_path_buf()),
            stream_fn: Some(provider.stream_fn()),
        })
        .unwrap()
    }

    pub fn cwd(&self) -> String {
        self.project_dir.path().display().to_string()
    }
}

/// One frame off a watch, with a timeout so a silent engine fails the test
/// instead of hanging it.
pub async fn next_frame<S>(stream: &mut S) -> serde_json::Value
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("timed out waiting for a watch frame")
        .expect("watch stream ended")
}

/// Configure the provider key and create the chat the runs target.
pub async fn setup_chat(engine: &LocalEngine, chat_id: &str) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": chat_id }),
        )
        .await
        .unwrap();
}

/// Queue a run command exactly as the composer serializes it.
pub async fn run_prompt(engine: &LocalEngine, chat_id: &str, cwd: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": format!("message-{}", uuid_tag(prompt)),
                    "request": {
                        "prompt": prompt,
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
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

/// A filesystem-safe stand-in for a message id (prompt text is not).
fn uuid_tag(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("m{hash:x}")
}

/// Pump session frames until `chat_id` carries `status`.
pub async fn wait_for_session_status<S>(sessions: &mut S, chat_id: &str, status: &str)
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    loop {
        let frame = next_frame(sessions).await;
        let hit = frame.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["chatId"] == chat_id && row["status"] == status)
        });
        if hit {
            return;
        }
    }
}

/// Pump transcript frames until some entry or append carries `needle` (a
/// text or error part, or a streaming append).
pub async fn wait_for_transcript_text<S>(transcript: &mut S, needle: &str)
where
    S: StreamExt<Item = serde_json::Value> + Unpin,
{
    loop {
        let frame = next_frame(transcript).await;
        if frame.to_string().contains(needle) {
            return;
        }
    }
}

/// Poll the scripted provider until it has received `count` requests.
pub async fn wait_for_requests(provider: &ScriptedProvider, count: usize) {
    let deadline = std::time::Instant::now() + WAIT;
    while provider.requests().len() < count {
        if std::time::Instant::now() > deadline {
            panic!(
                "provider never received {count} requests (saw {})",
                provider.requests().len()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Subscribe both watches and drain their opening frames (the transcript's
/// whole-history `reset` — empty for a fresh chat, the persisted history
/// after a restart — and the sessions snapshot) so the first frame after a
/// queued command is signal, not noise.
pub async fn subscribe(
    engine: &LocalEngine,
    chat_id: &str,
) -> (
    impl StreamExt<Item = serde_json::Value> + Unpin + use<>,
    impl StreamExt<Item = serde_json::Value> + Unpin + use<>,
) {
    let RpcReply::Stream(mut transcript) = engine
        .handle(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchDocMessages did not return a stream");
    };
    let _ = transcript.next().await.expect("transcript watch ended");
    let RpcReply::Stream(mut sessions) = engine
        .handle(methods::WATCH_SESSIONS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSessions did not return a stream");
    };
    let _ = sessions.next().await.expect("sessions watch ended");
    (transcript, sessions)
}

/// The transcript watch's opening frame — the whole-history `reset` a
/// freshly opened chat replays (restored rows, notices and all).
pub async fn transcript_snapshot(engine: &LocalEngine, chat_id: &str) -> serde_json::Value {
    let RpcReply::Stream(mut transcript) = engine
        .handle(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchDocMessages did not return a stream");
    };
    transcript.next().await.expect("transcript watch ended")
}

// ---------------------------------------------------------------------------
// Request-message summaries — the assertion vocabulary for "what the model
// would receive"
// ---------------------------------------------------------------------------

/// Render one request's message list as comparable strings:
/// `user:<text>`, `assistant:<text>`, `assistant:toolcall:<id>`,
/// `toolresult:<id>:<text>`.
pub fn summarize(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::User(user) => format!("user:{}", user.content.text()),
            Message::Assistant(assistant) => {
                let mut parts: Vec<String> = assistant
                    .content
                    .iter()
                    .map(|block| match block {
                        AssistantContent::Text(text) => format!("text:{}", text.text),
                        AssistantContent::ToolCall(call) => format!("toolcall:{}", call.id),
                        AssistantContent::Thinking(_) => "thinking".to_string(),
                    })
                    .collect();
                if let Some(error) = &assistant.error_message {
                    parts.push(format!("error:{error}"));
                }
                format!("assistant:{}", parts.join("+"))
            }
            Message::ToolResult(result) => {
                let text: Vec<String> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        BlockContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect();
                format!("toolresult:{}:{}", result.tool_call_id, text.join("|"))
            }
        })
        .collect()
}
