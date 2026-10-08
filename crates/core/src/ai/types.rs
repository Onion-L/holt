//! Port of `pi-core/ai/src/types.ts`: messages, content blocks, usage, model
//! definitions, and the assistant message event protocol.
//!
//! Serialization parity notes:
//! - Every wire-visible struct derives serde with camelCase field names and
//!   omits optional fields exactly like TypeScript object spreads.
//! - Token counts are integers and costs are floats, matching JSON output of
//!   the TypeScript oracle (`Usage`).
//! - `Tool.parameters` is a TypeBox schema in TypeScript; Rust carries the raw
//!   JSON schema value.
//! - The four TypeScript `compat` interfaces are merged into one runtime
//!   [`ModelCompat`] struct: their fields are optional, overlap on names and
//!   meanings, and providers read only the fields relevant to their API.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::telemetry::TelemetryContext;

/// A JSON number that serializes like a JavaScript `number`: integral values
/// print without a fractional part (`0.0` becomes `0`), matching
/// `JSON.stringify` output of the TypeScript oracle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JsF64(pub f64);

impl Serialize for JsF64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
            serializer.serialize_i64(value as i64)
        } else {
            serializer.serialize_f64(value)
        }
    }
}

impl<'de> Deserialize<'de> for JsF64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?;
        Ok(JsF64(number.as_f64().unwrap_or_default()))
    }
}

impl From<f64> for JsF64 {
    fn from(value: f64) -> Self {
        JsF64(value)
    }
}

impl From<JsF64> for f64 {
    fn from(value: JsF64) -> Self {
        value.0
    }
}

/// Defines a struct whose serialization is the literal tag string of a
/// content-block `type` field, so each content struct carries its
/// discriminator exactly like the TypeScript objects.
macro_rules! content_type_tag {
    ($name:ident, $tag:literal) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($tag)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                if value == $tag {
                    Ok($name)
                } else {
                    Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(&value),
                        &$tag,
                    ))
                }
            }
        }
    };
}

content_type_tag!(TypeText, "text");
content_type_tag!(TypeThinking, "thinking");
content_type_tag!(TypeImage, "image");
content_type_tag!(TypeToolCall, "toolCall");

/// The literal `"assistant"` role tag carried by assistant messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoleAssistant;

impl Serialize for RoleAssistant {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("assistant")
    }
}

impl<'de> Deserialize<'de> for RoleAssistant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "assistant" {
            Ok(RoleAssistant)
        } else {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&value),
                &"assistant",
            ))
        }
    }
}

/// The literal `"user"` role tag carried by user messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoleUser;

impl Serialize for RoleUser {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("user")
    }
}

impl<'de> Deserialize<'de> for RoleUser {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "user" {
            Ok(RoleUser)
        } else {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&value),
                &"user",
            ))
        }
    }
}

/// The literal `"toolResult"` role tag carried by tool result messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoleToolResult;

impl Serialize for RoleToolResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("toolResult")
    }
}

impl<'de> Deserialize<'de> for RoleToolResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "toolResult" {
            Ok(RoleToolResult)
        } else {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&value),
                &"toolResult",
            ))
        }
    }
}

/// Port of `KnownApi`. Open string unions become `String` with the known
/// values as constants.
pub type Api = String;

pub mod known_api {
    pub const OPENAI_COMPLETIONS: &str = "openai-completions";
    pub const MISTRAL_CONVERSATIONS: &str = "mistral-conversations";
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    pub const AZURE_OPENAI_RESPONSES: &str = "azure-openai-responses";
    pub const OPENAI_CODEX_RESPONSES: &str = "openai-codex-responses";
    pub const ANTHROPIC_MESSAGES: &str = "anthropic-messages";
    pub const BEDROCK_CONVERSE_STREAM: &str = "bedrock-converse-stream";
    pub const GOOGLE_GENERATIVE_AI: &str = "google-generative-ai";
    pub const GOOGLE_VERTEX: &str = "google-vertex";
    pub const PI_MESSAGES: &str = "pi-messages";
}

/// Port of `KnownImagesApi`.
pub type ImagesApi = String;

pub mod known_images_api {
    pub const OPENROUTER_IMAGES: &str = "openrouter-images";
}

/// Port of `ProviderId`.
pub type ProviderId = String;

/// Port of `ImagesProviderId`.
pub type ImagesProviderId = String;

/// Port of `ToolChoice`: `"auto" | "any" | "none" | { type: "tool"; name }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

impl serde::Serialize for ToolChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolChoice::Auto => serializer.serialize_str("auto"),
            ToolChoice::Any => serializer.serialize_str("any"),
            ToolChoice::None => serializer.serialize_str("none"),
            ToolChoice::Tool { name } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "tool")?;
                map.serialize_entry("name", name)?;
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for ToolChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Tool {
                #[serde(rename = "type")]
                kind: String,
                name: String,
            },
        }
        match Repr::deserialize(deserializer)? {
            Repr::Str(value) => match value.as_str() {
                "auto" => Ok(ToolChoice::Auto),
                "any" => Ok(ToolChoice::Any),
                "none" => Ok(ToolChoice::None),
                other => Err(serde::de::Error::unknown_variant(
                    other,
                    &["auto", "any", "none"],
                )),
            },
            Repr::Tool { kind, name } if kind == "tool" => Ok(ToolChoice::Tool { name }),
            Repr::Tool { kind, .. } => Err(serde::de::Error::unknown_variant(&kind, &["tool"])),
        }
    }
}

/// Port of `ThinkingLevel`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Port of `ModelThinkingLevel` (`"off" | ThinkingLevel`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ModelThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ModelThinkingLevel {
    /// The extended thinking level this value maps to, if not `off`.
    pub fn as_thinking_level(self) -> Option<ThinkingLevel> {
        match self {
            ModelThinkingLevel::Off => None,
            ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
            ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
            ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
            ModelThinkingLevel::High => Some(ThinkingLevel::High),
            ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
            ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
        }
    }
}

impl From<ThinkingLevel> for ModelThinkingLevel {
    fn from(level: ThinkingLevel) -> Self {
        match level {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
            ThinkingLevel::Max => ModelThinkingLevel::Max,
        }
    }
}

/// Port of `ThinkingLevelMap`: missing keys use provider defaults, `null`
/// marks a level as unsupported. Map absence and a `null` value are distinct.
pub type ThinkingLevelMap = BTreeMap<ModelThinkingLevel, Option<String>>;

/// Port of `ChatTemplateKwargValue`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    Text(String),
    Number(f64),
    Boolean(bool),
    Null,
    Variable {
        #[serde(rename = "$var")]
        variable: ThinkingVariable,
        #[serde(rename = "omitWhenOff", skip_serializing_if = "Option::is_none")]
        omit_when_off: Option<bool>,
    },
}

/// The `$var` members of `ChatTemplateKwargValue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingVariable {
    #[serde(rename = "thinking.enabled")]
    Enabled,
    #[serde(rename = "thinking.effort")]
    Effort,
    #[serde(rename = "thinking.budget")]
    Budget,
}

/// Port of `ThinkingTokenBudgetField`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingTokenBudgetField {
    #[serde(rename = "thinking_token_budget")]
    ThinkingTokenBudget,
    #[serde(rename = "thinking_budget")]
    ThinkingBudget,
    #[serde(rename = "thinking_budget_tokens")]
    ThinkingBudgetTokens,
}

/// Port of `ThinkingBudgets`: token budgets per thinking level.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

/// Port of `CacheRetention`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// Port of `Transport`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

impl Transport {
    /// The kebab-case transport name (the TypeScript literal values).
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Sse => "sse",
            Transport::Websocket => "websocket",
            Transport::WebsocketCached => "websocket-cached",
            Transport::Auto => "auto",
        }
    }
}

/// Port of `ProviderEnv`.
pub type ProviderEnv = BTreeMap<String, String>;

/// Port of `ProviderHeaders`; a `None` value suppresses a default header.
pub type ProviderHeaders = BTreeMap<String, Option<String>>;

/// Port of `SessionAffinityFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAffinityFormat {
    Openai,
    #[serde(rename = "openai-nosession")]
    OpenaiNosession,
    Openrouter,
}

/// Port of `ProviderResponse`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Message content
// ---------------------------------------------------------------------------

/// Port of `TextContent`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    #[serde(rename = "type")]
    pub content_type: TypeText,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Port of `ThinkingContent`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub content_type: TypeThinking,
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// Phase metadata stored in an OpenAI Responses text signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSignaturePhase {
    Commentary,
    FinalAnswer,
}

/// Port of `TextSignatureV1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSignatureV1 {
    pub v: u8,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub phase: Option<TextSignaturePhase>,
}

impl TextSignatureV1 {
    pub fn new(id: impl Into<String>, phase: Option<TextSignaturePhase>) -> Self {
        Self {
            v: 1,
            id: id.into(),
            phase,
        }
    }
}

/// Port of `ImageContent`; `data` is base64-encoded image data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub content_type: TypeImage,
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Port of `ToolCall` arguments: a JSON object.
pub type ToolCallArguments = serde_json::Map<String, serde_json::Value>;

/// Port of `ToolCall`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub content_type: TypeToolCall,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: ToolCallArguments,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Assistant content blocks (`TextContent | ThinkingContent | ToolCall`).
///
/// Untagged: each member carries its own `type` discriminator field, matching
/// the TypeScript objects exactly in both standalone and inline positions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// User/tool-result content blocks (`TextContent | ImageContent`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockContent {
    Text(TextContent),
    Image(ImageContent),
}

/// Port of `UserMessage["content"]`: a plain string or content blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<BlockContent>),
}

impl UserContent {
    /// The text of a string content or the joined text blocks.
    pub fn text(&self) -> String {
        match self {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    BlockContent::Text(text) => Some(text.text.as_str()),
                    BlockContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

// ---------------------------------------------------------------------------
// Usage, stop reasons, deferred handles
// ---------------------------------------------------------------------------

/// Port of `Usage.cost`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: JsF64,
    pub output: JsF64,
    pub cache_read: JsF64,
    pub cache_write: JsF64,
    pub total: JsF64,
}

/// Port of `Usage`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Port of `StopReason`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// Port of `DeferredHandle.data`.
pub type DeferredData = serde_json::Value;

/// Port of `DeferredHandle`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(rename = "pollAfterMs", skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<DeferredData>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Port of `UserMessage`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub role: RoleUser,
    pub content: UserContent,
    pub timestamp: i64,
}

/// Port of `AssistantMessage`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub role: RoleAssistant,
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "rawStopReason", skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: i64,
}

/// Port of `ToolResultMessage<TDetails>`. Tool details are provider- or
/// tool-specific values carried as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub role: RoleToolResult,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub content: Vec<BlockContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "addedToolNames", skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    #[serde(rename = "isError")]
    pub is_error: bool,
    pub timestamp: i64,
}

/// Port of `Message`.
///
/// TypeScript discriminates members by their `role` property, which each
/// message type carries itself; the Rust enum forwards serialization to the
/// inner message and dispatches on `role` when deserializing.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    User(UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(Box<ToolResultMessage>),
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Message::User(message) => message.serialize(serializer),
            Message::Assistant(message) => message.serialize(serializer),
            Message::ToolResult(message) => message.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
            "user" => Ok(Message::User(parse(value)?)),
            "assistant" => Ok(Message::Assistant(parse(value)?)),
            "toolResult" => Ok(Message::ToolResult(parse(value)?)),
            other => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(other),
                &"one of \"user\", \"assistant\", \"toolResult\"",
            )),
        }
    }
}

impl Message {
    /// The message timestamp.
    pub fn timestamp(&self) -> i64 {
        match self {
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Port of `ImagesInputContent`/`ImagesOutputContent`.
pub type ImagesContent = BlockContent;

/// Port of `ImagesContext`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImagesContext {
    pub input: Vec<ImagesContent>,
}

/// Port of `ImagesStopReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

/// Port of `AssistantImages`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: ImagesApi,
    pub provider: ImagesProviderId,
    pub model: String,
    pub output: Vec<ImagesContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    pub stop_reason: ImagesStopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Port of `GrammarFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GrammarFormat {
    #[serde(rename = "openai_lark")]
    OpenaiLark,
    #[serde(rename = "openai_regex")]
    OpenaiRegex,
}

/// Port of `GrammarVariants`.
pub type GrammarVariants = BTreeMap<GrammarFormat, String>;

/// Port of `ConstrainedSamplingConfig`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    #[serde(rename = "json_schema")]
    JsonSchema { strict: ConstrainedSamplingStrict },
    #[serde(rename = "grammar")]
    Grammar { variants: GrammarVariants },
}

/// The `strict` member of the JSON-schema constrained sampling config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstrainedSamplingStrict {
    Prefer,
    Require,
}

/// Port of `Tool.constrainedSampling`: `false` disables constrained sampling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolConstrainedSampling {
    Disabled(bool),
    Config(ConstrainedSamplingConfig),
}

/// Port of `Tool`. TypeScript carries a TypeBox schema; Rust carries the raw
/// JSON schema value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ToolConstrainedSampling>,
}

/// Port of `Context`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

// ---------------------------------------------------------------------------
// Assistant message events
// ---------------------------------------------------------------------------

/// Reason carried by `done` events (`"stop" | "length" | "toolUse" | "deferred"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneReason {
    Stop,
    Length,
    ToolUse,
    Deferred,
}

impl From<DoneReason> for StopReason {
    fn from(reason: DoneReason) -> Self {
        match reason {
            DoneReason::Stop => StopReason::Stop,
            DoneReason::Length => StopReason::Length,
            DoneReason::ToolUse => StopReason::ToolUse,
            DoneReason::Deferred => StopReason::Deferred,
        }
    }
}

/// Reason carried by `error` events (`"aborted" | "error"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorReason {
    Aborted,
    Error,
}

impl From<ErrorReason> for StopReason {
    fn from(reason: ErrorReason) -> Self {
        match reason {
            ErrorReason::Aborted => StopReason::Aborted,
            ErrorReason::Error => StopReason::Error,
        }
    }
}

/// Port of `AssistantMessageEvent`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    #[serde(rename = "start")]
    Start { partial: AssistantMessage },
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    #[serde(rename = "done")]
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    #[serde(rename = "error")]
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// The terminal-event check used by `AssistantMessageEventStream`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }

    /// The final assistant message carried by a terminal event.
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            AssistantMessageEvent::Done { message, .. } => Some(message),
            AssistantMessageEvent::Error { error, .. } => Some(error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostics (port of utils/diagnostics.ts types)
// ---------------------------------------------------------------------------

/// Port of `DiagnosticErrorInfo`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<DiagnosticErrorCode>,
}

/// Port of the `string | number` diagnostic error code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DiagnosticErrorCode {
    Text(String),
    Number(f64),
}

/// Port of `AssistantMessageDiagnostic`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Compat settings (merged runtime shape of the four TS compat interfaces)
// ---------------------------------------------------------------------------

/// Port of `ChatTemplateKwargValue` maps (`chatTemplateKwargs`/`chatTemplateArgs`).
pub type ChatTemplateKwargs = BTreeMap<String, ChatTemplateKwargValue>;

/// Port of `OpenRouterRouting`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterRouting {
    #[serde(rename = "allow_fallbacks", skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(rename = "require_parameters", skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(rename = "data_collection", skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    #[serde(
        rename = "enforce_distillable_text",
        skip_serializing_if = "Option::is_none"
    )]
    pub enforce_distillable_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<serde_json::Value>,
    #[serde(rename = "max_price", skip_serializing_if = "Option::is_none")]
    pub max_price: Option<serde_json::Value>,
    #[serde(
        rename = "preferred_min_throughput",
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_min_throughput: Option<serde_json::Value>,
    #[serde(
        rename = "preferred_max_latency",
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_max_latency: Option<serde_json::Value>,
}

/// Port of `VercelGatewayRouting`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VercelGatewayRouting {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}

/// Port of `AnthropicAllowedFallbackModel`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicAllowedFallbackModel {
    pub provider: ProviderId,
    pub model: String,
    pub cost: ModelCost,
}

/// Port of `ThinkingTokenBudgetField` compat values and the merged runtime
/// shape of `OpenAICompletionsCompat`, `OpenAIResponsesCompat`,
/// `AnthropicMessagesCompat`, and `BedrockCompat`. TypeScript narrows these by
/// API at compile time; at runtime providers read the fields they know and
/// absent fields stay `undefined`, which `Option::None` reproduces.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompat {
    // OpenAICompletionsCompat / OpenAIResponsesCompat
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<ChatTemplateKwargs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_args: Option<ChatTemplateKwargs>,
    #[serde(rename = "openRouterRouting", skip_serializing_if = "Option::is_none")]
    pub open_router_routing: Option<OpenRouterRouting>,
    #[serde(
        rename = "vercelGatewayRouting",
        skip_serializing_if = "Option::is_none"
    )]
    pub vercel_gateway_routing: Option<VercelGatewayRouting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    #[serde(
        rename = "thinkingTokenBudgetField",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_thinking_token_budget: Option<bool>,
    #[serde(
        rename = "supportsOpenAIGrammarTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_open_ai_grammar_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(rename = "cacheControlFormat", skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<CacheControlFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(rename = "deferredToolsMode", skip_serializing_if = "Option::is_none")]
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    #[serde(
        rename = "sessionAffinityFormat",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,

    // OpenAIResponsesCompat-only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_additional_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_search: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_explicit_prompt_cache_mode: Option<bool>,

    // AnthropicMessagesCompat-only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    #[serde(
        rename = "allowEmptySignature",
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_empty_signature: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_tools: Option<bool>,
    #[serde(
        rename = "allowedFallbackModels",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_fallback_models: Option<Vec<AnthropicAllowedFallbackModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_references: Option<bool>,
}

/// Port of `maxTokensField`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaxTokensField {
    #[serde(rename = "max_completion_tokens")]
    MaxCompletionTokens,
    #[serde(rename = "max_tokens")]
    MaxTokens,
}

/// Port of `cacheControlFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheControlFormat {
    Anthropic,
}

/// Port of `deferredToolsMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferredToolsMode {
    Kimi,
}

/// Port of the `thinkingFormat` union.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    Openai,
    Openrouter,
    Deepseek,
    Together,
    Baseten,
    Zai,
    Qwen,
    #[serde(rename = "chat-template")]
    ChatTemplate,
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    #[serde(rename = "string-thinking")]
    StringThinking,
    #[serde(rename = "ant-ling")]
    AntLing,
}

// ---------------------------------------------------------------------------
// Model definitions
// ---------------------------------------------------------------------------

/// Port of `ModelCostRates`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: JsF64,
    pub output: JsF64,
    #[serde(rename = "cacheRead")]
    pub cache_read: JsF64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: JsF64,
}

/// Port of `ModelCostTier`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(rename = "inputTokensAbove")]
    pub input_tokens_above: u64,
}

/// Port of `ModelCost`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

/// Port of `Model<TApi>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(rename = "name")]
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub reasoning: bool,
    #[serde(rename = "thinkingLevelMap", skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u64,
    #[serde(rename = "samplingParams", skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<ModelCompat>,
    /// Preserves unknown JSON fields through round-trips, mirroring the
    /// permissive TypeScript object shapes.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty", default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Port of the `("text" | "image")[]` model input list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

/// Port of `ImagesModel<TApi>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    pub id: String,
    #[serde(rename = "name")]
    pub name: String,
    pub api: ImagesApi,
    pub provider: ImagesProviderId,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub input: Vec<ModelInput>,
    pub output: Vec<ModelInput>,
    pub cost: ModelCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty", default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Request options (not serialized; runtime-only option structs)
// ---------------------------------------------------------------------------

/// Port of the `fetch` option: an injectable HTTP transport. The concrete
/// trait is introduced with the provider HTTP layer.
pub type FetchFunction = Arc<dyn crate::ai::utils::http::HttpFetch>;

/// Port of `ProviderRequestOptions<TModel>`.
#[derive(Clone, Default)]
pub struct ProviderRequestOptions {
    pub signal: Option<tokio_util::sync::CancellationToken>,
    /// Explicit parent context for telemetry produced by this logical request.
    pub telemetry_context: Option<Arc<dyn TelemetryContext>>,
    pub api_key: Option<String>,
    pub fetch: Option<FetchFunction>,
    pub env: Option<ProviderEnv>,
    /// Inspects or replaces provider payloads before sending; returning
    /// `None` keeps the payload unchanged.
    pub on_payload: Option<OnPayloadCallback>,
    /// Invoked after an HTTP response is received.
    pub on_response: Option<OnResponseCallback>,
    /// Invoked before each provider-retry backoff sleep with (attempt
    /// 1-indexed, max_retries, delay_ms, error message).
    pub on_retry: Option<OnRetryCallback>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    /// Preserves unknown TypeScript option fields for forward compatibility.
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Port of the `onPayload` callback.
pub type OnPayloadCallback = Arc<
    dyn Fn(
            serde_json::Value,
            &Model,
        ) -> futures::future::BoxFuture<'static, Option<serde_json::Value>>
        + Send
        + Sync,
>;

/// Port of the `onResponse` callback.
pub type OnResponseCallback =
    Arc<dyn Fn(&ProviderResponse, &Model) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Port of the `onRetry` callback: fires once per scheduled provider retry,
/// just before the backoff sleep, with the same argument shape as the
/// harness-level `onRetryScheduled` (attempt 1-indexed, max_retries,
/// delay_ms, previous error message).
pub type OnRetryCallback = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

/// Port of the images `onPayload` callback (`ImagesOptions` flavor).
pub type OnPayloadCallbackImages = Arc<
    dyn Fn(
            serde_json::Value,
            &ImagesModel,
        ) -> futures::future::BoxFuture<'static, Option<serde_json::Value>>
        + Send
        + Sync,
>;

/// Port of the images `onResponse` callback (`ImagesOptions` flavor).
pub type OnResponseCallbackImages = Arc<
    dyn Fn(&ProviderResponse, &ImagesModel) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Port of `ImagesOptions`: `ProviderRequestOptions<ImagesModel>` plus the
/// images-specific `metadata` field.
#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub signal: Option<tokio_util::sync::CancellationToken>,
    pub telemetry_context: Option<Arc<dyn TelemetryContext>>,
    pub api_key: Option<String>,
    pub fetch: Option<FetchFunction>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<OnPayloadCallbackImages>,
    pub on_response: Option<OnResponseCallbackImages>,
    /// Invoked before each provider-retry backoff sleep; see
    /// [`ProviderRequestOptions::on_retry`].
    pub on_retry: Option<OnRetryCallback>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    /// Preserves unknown TypeScript option fields for forward compatibility.
    pub extra: BTreeMap<String, serde_json::Value>,
    /// Optional metadata to include in API requests.
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
}

/// Port of `ModelsRequestTransforms.transformHeaders`: transforms the fully
/// assembled model/auth/request headers before provider dispatch.
pub type TransformHeadersFn = Arc<
    dyn Fn(ProviderHeaders) -> futures::future::BoxFuture<'static, ProviderHeaders> + Send + Sync,
>;

/// Port of `StreamOptions`, carrying the Models-level
/// `ModelsRequestTransforms.transformHeaders` extension (applied by the
/// `Models` collection and ignored by provider implementations).
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub base: ProviderRequestOptions,
    pub temperature: Option<f64>,
    pub sampling_params: Option<BTreeMap<String, serde_json::Value>>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    pub transform_headers: Option<TransformHeadersFn>,
}

/// Port of `DeferredFetchOptions`: request options plus the maximum
/// provider long-poll duration in milliseconds (0 = one status check).
#[derive(Clone, Default)]
pub struct DeferredFetchOptions {
    pub base: ProviderRequestOptions,
    pub wait: Option<u64>,
}

/// Port of `SimpleStreamOptions`.
#[derive(Clone, Default)]
pub struct SimpleStreamOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub deferred: Option<DeferredPreference>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Port of `SimpleStreamOptions.deferred`.
#[derive(Clone, Debug, PartialEq)]
pub enum DeferredPreference {
    Enabled,
    Window(DeferredWindow),
}

/// The deferred window values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredWindow {
    M15,
    H1,
    H24,
}
