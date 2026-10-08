//! Port of `pi-core/agent/src/types.ts`.

use std::collections::HashSet;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::ai::types::{
    AssistantMessage, BlockContent, CacheRetention, Context, Message, Model, SimpleStreamOptions,
    ThinkingBudgets, ThinkingLevel as ProviderThinkingLevel, Tool, ToolCall,
    ToolConstrainedSampling, ToolResultMessage, Transport, Usage,
};

/// Wall-clock milliseconds, the Rust stand-in for `Date.now()`.
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Port of `StreamFn`.
///
/// Contract mirrors the TypeScript doc comment: conforming implementations
/// must not fail for request/model/runtime failures — those belong in the
/// returned stream as protocol events plus a final `error`/`aborted`
/// assistant message. The `Result` return is the Rust analog of a
/// TypeScript stream function that throws (a contract violation); the
/// failure escapes the loop exactly like an uncaught throw.
pub type StreamFn = Arc<
    dyn Fn(
            &Model,
            &Context,
            Option<&SimpleStreamOptions>,
        ) -> Result<AssistantMessageEventStream, String>
        + Send
        + Sync,
>;

/// Alias re-exported for the stream type produced by [`StreamFn`].
pub use crate::ai::utils::event_stream::AssistantMessageEventStream;

/// Port of `ToolExecutionMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// Port of `QueueMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Port of the agent-side `ThinkingLevel` (`"off"` plus the provider
/// reasoning levels).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// Maps to the provider reasoning level carried by
    /// `SimpleStreamOptions.reasoning`; `"off"` maps to `None`.
    pub fn as_provider_reasoning(self) -> Option<ProviderThinkingLevel> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Minimal => Some(ProviderThinkingLevel::Minimal),
            ThinkingLevel::Low => Some(ProviderThinkingLevel::Low),
            ThinkingLevel::Medium => Some(ProviderThinkingLevel::Medium),
            ThinkingLevel::High => Some(ProviderThinkingLevel::High),
            ThinkingLevel::Xhigh => Some(ProviderThinkingLevel::Xhigh),
            ThinkingLevel::Max => Some(ProviderThinkingLevel::Max),
        }
    }
}

/// Read-only snapshot of the public agent state.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<AgentTool>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: HashSet<String>,
    pub error_message: Option<String>,
}

/// A single tool call content block emitted by an assistant message.
pub type AgentToolCall = ToolCall;

/// Port of `BeforeToolCallResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: Option<bool>,
    pub reason: Option<String>,
    pub terminate: Option<bool>,
}

/// Port of `AfterToolCallResult`.
///
/// Field-by-field merge semantics: `None` keeps the executed value, `Some`
/// replaces it wholesale.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<BlockContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// A custom application message. TypeScript models these through interface
/// merging on `CustomAgentMessages`; the Rust port carries the message role
/// plus the full JSON payload so arbitrary shapes round-trip.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomAgentMessage {
    /// The message role, e.g. `"notification"` or `"bashExecution"`.
    pub role: String,
    /// The complete message payload (includes `role`).
    pub value: serde_json::Value,
}

impl CustomAgentMessage {
    /// Builds a custom message from a role and payload fields (without
    /// `role`, which is added automatically).
    pub fn new(role: impl Into<String>, fields: serde_json::Value) -> Self {
        let role = role.into();
        let mut object = fields;
        if let serde_json::Value::Object(map) = &mut object {
            let mut with_role = serde_json::Map::new();
            with_role.insert("role".to_string(), serde_json::Value::String(role.clone()));
            with_role.append(map);
            object = serde_json::Value::Object(with_role);
        }
        Self {
            role,
            value: object,
        }
    }
}

/// Port of `AgentMessage`: LLM messages plus custom application messages.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentMessage {
    User(crate::ai::types::UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(Box<ToolResultMessage>),
    Custom(CustomAgentMessage),
}

impl From<crate::ai::types::UserMessage> for AgentMessage {
    fn from(message: crate::ai::types::UserMessage) -> Self {
        AgentMessage::User(message)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(message: AssistantMessage) -> Self {
        AgentMessage::Assistant(Box::new(message))
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(message: ToolResultMessage) -> Self {
        AgentMessage::ToolResult(Box::new(message))
    }
}

impl From<CustomAgentMessage> for AgentMessage {
    fn from(message: CustomAgentMessage) -> Self {
        AgentMessage::Custom(message)
    }
}

impl serde::Serialize for AgentMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AgentMessage::User(message) => message.serialize(serializer),
            AgentMessage::Assistant(message) => message.serialize(serializer),
            AgentMessage::ToolResult(message) => message.serialize(serializer),
            AgentMessage::Custom(message) => message.value.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for AgentMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let role = value
            .get("role")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::missing_field("role"))?;
        fn parse<T: serde::de::DeserializeOwned, E: serde::de::Error>(
            value: serde_json::Value,
        ) -> Result<T, E> {
            serde_json::from_value(value).map_err(|error| E::custom(error.to_string()))
        }
        match role {
            "user" => Ok(AgentMessage::User(parse(value)?)),
            "assistant" => Ok(AgentMessage::Assistant(parse(value)?)),
            "toolResult" => Ok(AgentMessage::ToolResult(parse(value)?)),
            role => Ok(AgentMessage::Custom(CustomAgentMessage {
                role: role.to_string(),
                value,
            })),
        }
    }
}

impl AgentMessage {
    /// The message role discriminator.
    pub fn role(&self) -> &str {
        match self {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::Custom(message) => &message.role,
        }
    }

    /// Converts a standard LLM message into an agent message.
    pub fn from_llm_message(message: Message) -> AgentMessage {
        match message {
            Message::User(message) => AgentMessage::User(message),
            Message::Assistant(message) => AgentMessage::Assistant(message),
            Message::ToolResult(message) => AgentMessage::ToolResult(message),
        }
    }

    /// Converts the agent message into a standard LLM message, if it is one.
    pub fn as_llm_message(&self) -> Option<Message> {
        match self {
            AgentMessage::User(message) => Some(Message::User(message.clone())),
            AgentMessage::Assistant(message) => Some(Message::Assistant(message.clone())),
            AgentMessage::ToolResult(message) => Some(Message::ToolResult(message.clone())),
            AgentMessage::Custom(_) => None,
        }
    }

    /// Consuming variant of [`AgentMessage::as_llm_message`].
    pub fn into_llm_message(self) -> Option<Message> {
        match self {
            AgentMessage::User(message) => Some(Message::User(message)),
            AgentMessage::Assistant(message) => Some(Message::Assistant(message)),
            AgentMessage::ToolResult(message) => Some(Message::ToolResult(message)),
            AgentMessage::Custom(_) => None,
        }
    }
}

/// Port of `AgentToolResult<T>`; details ride as JSON.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentToolResult {
    pub content: Vec<BlockContent>,
    pub details: serde_json::Value,
    pub usage: Option<Usage>,
    pub added_tool_names: Option<Vec<String>>,
    pub terminate: Option<bool>,
}

/// Port of `AgentToolUpdateCallback`.
///
/// Scoped to the current `execute()` invocation; calls made after the tool
/// future settles are ignored.
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// The `execute` member of `AgentTool`.
pub type AgentToolExecuteFn = Arc<
    dyn Fn(
            &str,
            &serde_json::Value,
            Option<&CancellationToken>,
            Option<&AgentToolUpdateCallback>,
        ) -> BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// Port of `prepareArguments`.
pub type PrepareArgumentsFn = Arc<dyn Fn(&serde_json::Value) -> serde_json::Value + Send + Sync>;

/// Port of `AgentTool`.
#[derive(Clone)]
pub struct AgentTool {
    pub name: String,
    /// Human-readable label for UI display.
    pub label: String,
    pub description: String,
    /// JSON schema for the tool parameters.
    pub parameters: serde_json::Value,
    pub constrained_sampling: Option<ToolConstrainedSampling>,
    /// Compatibility shim applied to raw tool-call arguments before schema
    /// validation.
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    pub execute: AgentToolExecuteFn,
    /// Per-tool execution mode override.
    pub execution_mode: Option<ToolExecutionMode>,
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl AgentTool {
    /// The plain provider-facing tool description (`Context.tools` entries).
    pub fn to_tool(&self) -> Tool {
        Tool {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            constrained_sampling: self.constrained_sampling.clone(),
        }
    }
}

/// Port of `AgentContext`.
#[derive(Clone, Debug, Default)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Option<Vec<AgentTool>>,
}

/// Port of `BeforeToolCallContext`.
#[derive(Clone, Debug)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    /// Validated arguments. TypeScript passes the shared validated object,
    /// so hooks can mutate it in place; the Rust port shares the same value
    /// behind a lock and the loop executes the current contents without
    /// revalidation.
    pub args: std::sync::Arc<std::sync::Mutex<serde_json::Value>>,
    pub context: AgentContext,
}

/// Port of `AfterToolCallContext`.
#[derive(Clone, Debug)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    /// The validated arguments (snapshot at call time).
    pub args: serde_json::Value,
    /// The executed tool result before overrides are applied.
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

/// Port of `ShouldStopAfterTurnContext` (also `PrepareNextTurnContext`).
#[derive(Clone, Debug)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    /// Messages this loop invocation will return if it exits at this point.
    pub new_messages: Vec<AgentMessage>,
}

/// Context type seen by `prepareNextTurn` callbacks.
pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// Port of `AgentLoopTurnUpdate`.
#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Port of `convertToLlm`.
pub type ConvertToLlmFn =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;

/// Port of `transformContext`.
pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, Option<CancellationToken>) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

/// Port of `getApiKey`.
pub type GetApiKeyFn = Arc<dyn Fn(&str) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Port of `shouldStopAfterTurn`.
pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync>;

/// Port of `prepareNextTurn`.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync,
>;

/// Port of `getSteeringMessages` / `getFollowUpMessages`.
pub type GetMessagesFn = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

/// Port of `beforeToolCall`.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

/// Port of `afterToolCall`.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, Option<AfterToolCallResult>>
        + Send
        + Sync,
>;

/// Port of `AgentLoopConfig` (extends `SimpleStreamOptions`).
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// The `SimpleStreamOptions` fields forwarded to every provider request.
    pub stream_options: SimpleStreamOptions,
    pub model: Model,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    pub tool_execution: Option<ToolExecutionMode>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
}

/// Port of `AgentEvent`.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Box<AgentMessage>,
    },
    MessageUpdate {
        message: Box<AgentMessage>,
        assistant_message_event: crate::ai::types::AssistantMessageEvent,
    },
    MessageEnd {
        message: Box<AgentMessage>,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: Box<AgentToolResult>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: Box<AgentToolResult>,
        is_error: bool,
    },
}

/// Serializable stream options carried by the proxy request
/// (`ProxySerializableStreamOptions` in `proxy.ts`).
#[derive(Clone, Debug, Default)]
pub struct ProxySerializableStreamOptions {
    pub temperature: Option<f64>,
    pub sampling_params: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    pub max_tokens: Option<u64>,
    pub reasoning: Option<ProviderThinkingLevel>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub headers: Option<crate::ai::types::ProviderHeaders>,
    pub metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    pub transport: Option<Transport>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub max_retry_delay_ms: Option<u64>,
}
