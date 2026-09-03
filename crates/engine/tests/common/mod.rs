//! Shared fixture for the scripted-provider test seam: a fake model
//! transport injected through `EngineConfig::stream_fn` so RPC-handle tests
//! drive a full Turn without a real provider. The provider records the
//! message list of every request it receives and replies from a script —
//! text, tool calls, an aborted stream, a provider error — with fixed usage
//! numbers so token-dependent behavior stays deterministic.
//!
//! Every later History/Compaction test rides this seam; no production code
//! path depends on it.

#![allow(dead_code)] // each test binary links the module whole

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pi_core::agent::types::StreamFn;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Message, Model, StopReason, TextContent, ToolCall, Usage,
};

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
    /// partial content lands, stop reason `aborted`.
    Aborted { partial: String },
    /// A provider failure: the error string rides the assistant message,
    /// stop reason `error`.
    Failed(String),
}

impl ScriptedReply {
    pub fn text(text: impl Into<String>) -> Self {
        ScriptedReply::Text(text.into())
    }

    /// A single tool call with the given id, name, and JSON arguments.
    pub fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Self {
        ScriptedReply::ToolCalls(vec![tool_call(id, name, arguments)])
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
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
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
            requests.lock().unwrap().push(context.messages.clone());
            let reply = script.lock().unwrap().pop_front().unwrap_or_else(|| {
                ScriptedReply::Failed("scripted provider ran out of replies".into())
            });
            let stream = pi_core::ai::utils::event_stream::create_assistant_message_event_stream();
            push_reply(&stream, reply, model, &usage);
            Ok(stream)
        })
    }

    /// The message list of each request, in arrival order — what the model
    /// would receive.
    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
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
        ScriptedReply::Aborted { partial } => {
            // The half-streamed shape: a start with the partial content,
            // then the abort terminal carrying the same content — mirroring
            // the real transports' aborted message (stop reason `aborted`,
            // "Request was aborted" as its error message).
            message.content = vec![AssistantContent::Text(TextContent {
                text: partial,
                ..Default::default()
            })];
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
    }
}
