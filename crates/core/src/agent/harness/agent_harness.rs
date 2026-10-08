//! Port of `pi-core/agent/src/harness/agent-harness.ts` (the v2 scaffold).
//!
//! The upstream scaffold implements the configuration surface and
//! defensively rejects every not-yet-implemented operation with
//! `HarnessNotImplemented` (or `HarnessClosed` after `close()`); the Rust
//! port reproduces exactly that behavior with `Result` errors.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::agent::harness::compaction::compaction::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS,
};
use crate::agent::harness::session::memory::Session;
use crate::agent::harness::session::types::Entry;
use crate::agent::harness::types::{PromptTemplate, Skill};
use crate::agent::types::{AgentMessage, QueueMode, ThinkingLevel};
use crate::ai::models::Models;
use crate::ai::types::{
    AssistantMessage, DeferredHandle, ImageContent, Message, Model, SimpleStreamOptions, Usage,
};
use crate::ai::utils::retry::RetryPolicy;

/// The tagged rejection errors (port of the `TaggedError` subclasses).
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessRejection {
    LaneBusy {
        lane: String,
        operation_id: String,
    },
    MissingIdentities {
        lane: String,
        tools: Vec<String>,
        models: Vec<String>,
    },
    NoActiveRun {
        lane: String,
    },
    NoActiveOperation {
        lane: String,
    },
    NothingToResume {
        lane: String,
    },
    InvalidMessage {
        lane: String,
        reason: String,
    },
    UnknownSkill {
        name: String,
    },
    UnknownTemplate {
        name: String,
    },
    UnknownTarget {
        target_id: String,
    },
    UnknownQueueItem {
        lane: String,
        entry_id: String,
    },
    LaneExists {
        lane: String,
    },
    InvalidLane {
        lane: String,
        reason: String,
    },
    NothingToCompact {
        lane: String,
    },
    Closed,
}

/// Port of `HarnessFault`.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessFault {
    pub message: String,
    pub cause: String,
}

/// Port of `HarnessClosed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessClosed;

impl std::fmt::Display for HarnessClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness was closed while the operation was active")
    }
}

/// Port of `HarnessNotImplemented`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessNotImplemented {
    pub operation: String,
}

impl HarnessNotImplemented {
    fn new(operation: &str) -> Self {
        Self {
            operation: operation.to_string(),
        }
    }
}

impl std::fmt::Display for HarnessNotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness.{} is not implemented yet", self.operation)
    }
}

/// The error returned by unfinished scaffold operations.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessScaffoldError {
    Closed(HarnessClosed),
    NotImplemented(HarnessNotImplemented),
}

impl std::fmt::Display for HarnessScaffoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HarnessScaffoldError::Closed(error) => write!(f, "{error}"),
            HarnessScaffoldError::NotImplemented(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for HarnessScaffoldError {}

/// Port of `OperationError`.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// Port of `RunOutcome`.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    Completed {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Aborted {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
        final_entry_id: Option<String>,
        final_message: Option<AssistantMessage>,
    },
    Suspended {
        leaf_id: String,
        final_entry_id: String,
        deferred: DeferredHandle,
    },
}

/// Port of `CompactionOutcome`.
#[derive(Clone, Debug)]
pub enum CompactionOutcome {
    Completed {
        leaf_id: String,
        entry: Box<Entry>,
    },
    Declined {
        leaf_id: String,
    },
    Aborted {
        leaf_id: String,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
    },
}

/// Port of `NavigationOutcome`.
#[derive(Clone, Debug)]
pub enum NavigationOutcome {
    Completed {
        new_leaf_id: Option<String>,
        summary_entry: Option<Box<Entry>>,
    },
    Declined {
        leaf_id: Option<String>,
    },
    Aborted {
        leaf_id: Option<String>,
    },
    Failed {
        leaf_id: Option<String>,
        error: OperationError,
    },
}

/// Port of `RunResult`.
pub type RunResult = Result<RunWithId, HarnessRejection>;
/// A run result carrying its durable run id.
pub struct RunWithId {
    pub run_id: String,
    pub outcome: RunOutcome,
}

/// Port of `CompactionResult`.
pub type CompactionResult = Result<CompactionWithId, HarnessRejection>;
/// A compaction result carrying its durable run id.
pub struct CompactionWithId {
    pub run_id: String,
    pub outcome: CompactionOutcome,
}

/// Port of `NavigationResult`.
pub type NavigationResult = Result<NavigationWithId, HarnessRejection>;
/// A navigation result carrying its durable run id.
pub struct NavigationWithId {
    pub run_id: String,
    pub outcome: NavigationOutcome,
}

/// Port of `ResumeOutcome` / `ResumeResult`.
pub enum ResumeOutcome {
    Run {
        run_id: String,
        outcome: Box<RunOutcome>,
    },
    Compaction {
        run_id: String,
        outcome: Box<CompactionOutcome>,
    },
    Navigation {
        run_id: String,
        outcome: Box<NavigationOutcome>,
    },
}

/// Port of `QueueResult`.
pub type QueueResult = Result<QueuedEntry, HarnessRejection>;
/// A queued message carrying its durable entry id.
#[derive(Clone, Debug)]
pub struct QueuedEntry {
    pub entry_id: String,
    pub message: AgentMessage,
}

/// TypeScript name for [`QueuedEntry`].
pub type QueuedItem = QueuedEntry;

/// Port of `RecordUsageResult`.
pub type RecordUsageResult = Result<(), HarnessRejection>;

/// Port of `CancelQueuedResult`.
pub enum CancelQueuedOutcome {
    Cancelled,
    AlreadyConsumed,
    AlreadyCleared,
}

/// Port of `AbortResult`.
#[derive(Clone, Debug)]
pub struct AbortedRun {
    pub run_id: String,
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

/// Port of `SuspendedOperation`.
#[derive(Clone, Debug)]
pub struct SuspendedOperation {
    pub lane: String,
    /// `"run"`, `"compaction"`, or `"navigation"`.
    pub kind: &'static str,
    pub id: String,
    pub started_at: i64,
    /// `"crash"` or `"deferred"`.
    pub reason: &'static str,
    pub prompt: Option<Vec<AgentMessage>>,
    pub deferred: Option<DeferredHandle>,
    pub aborting: Option<AbortedRun>,
    pub missing_tools: Vec<String>,
    pub missing_models: Vec<String>,
}

/// Port of `LaneInfo`.
#[derive(Clone, Debug)]
pub struct LaneInfo {
    pub name: String,
    pub leaf_id: Option<String>,
    /// id, kind, and status of the open operation.
    pub operation: Option<LaneOperationInfo>,
}

/// The operation slice of [`LaneInfo`].
#[derive(Clone, Debug)]
pub struct LaneOperationInfo {
    pub id: String,
    /// `"run"`, `"compaction"`, or `"navigation"`.
    pub kind: &'static str,
    /// `"running"`, `"suspended"`, or `"aborting"`.
    pub status: &'static str,
}

/// Port of `NavigateOptions`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigateOptions {
    pub summarize: Option<bool>,
    pub custom_instructions: Option<String>,
    pub label: Option<String>,
}

/// Port of the inline `compact` options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessCompactOptions {
    pub custom_instructions: Option<String>,
}

/// Port of the inline `recordUsage` options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordUsageOptions {
    pub entry_id: Option<String>,
    pub details: Option<serde_json::Value>,
}

/// A prompt overload expressed as one Rust input enum.
#[derive(Clone, Debug)]
pub enum PromptInput {
    Text {
        text: String,
        images: Option<Vec<ImageContent>>,
    },
    Message(AgentMessage),
    Messages(Vec<AgentMessage>),
}

/// A steer/follow-up/next-run overload expressed as one Rust input enum.
#[derive(Clone, Debug)]
pub enum QueueInput {
    Text {
        text: String,
        images: Option<Vec<ImageContent>>,
    },
    Message(AgentMessage),
}

/// Tool replay policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolReplay {
    Never,
    Safe,
}

/// Harness driving policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveMode {
    Automatic,
    Manual,
}

/// Harness-wide tool execution policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecution {
    Sequential,
    Parallel,
}

/// `HarnessTool` is the public v2 name for the context-aware tool shape.
pub type HarnessTool = crate::agent::harness::types::AgentHarnessTool;

pub type EntryProjector = Arc<dyn Fn(Entry) -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;
pub type ToProviderMessages =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
#[derive(Clone)]
pub enum ToolContextSource {
    Value(crate::agent::harness::types::AgentToolContext),
    Dynamic(
        Arc<
            dyn Fn() -> BoxFuture<'static, crate::agent::harness::types::AgentToolContext>
                + Send
                + Sync,
        >,
    ),
}

#[derive(Clone)]
pub enum SystemPromptSource {
    Value(String),
    Dynamic(Arc<dyn Fn() -> BoxFuture<'static, String> + Send + Sync>),
}
pub type IdleCallback = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookOptions {
    pub id: Option<String>,
}

pub type HookHandler =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, serde_json::Value> + Send + Sync>;

/// Hook registry contract exposed by the scaffold.
pub trait Hooks: Send + Sync {
    fn on(
        &self,
        name: &str,
        handler: HookHandler,
        options: Option<HookOptions>,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, HarnessScaffoldError>;
}

/// Port of `ActionInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionInfo {
    AppendEntry {
        entry_type: String,
        entry_id: String,
    },
    AppendRecord {
        record_type: String,
    },
    MoveLane {
        to: Option<String>,
    },
    SetFact {
        fact: String,
    },
    TryFinishRun {
        outcome: String,
    },
    FinishOperation {
        outcome: String,
    },
    CommitFollowUp,
    ConsumeQueueItem {
        queue: String,
        entry_id: String,
    },
    ApplyPendingWrite {
        entry_id: String,
    },
    StreamAssistant {
        step: String,
        attempt: u32,
    },
    ExecuteTool {
        tool_call_id: String,
        tool_name: String,
    },
    FetchDeferred {
        provider: String,
        id: String,
    },
    CancelDeferred {
        provider: String,
        id: String,
    },
    Hook {
        name: String,
    },
    Sleep {
        delay_ms: u64,
    },
}

#[derive(Clone, Debug)]
pub struct PendingWrite {
    pub id: String,
    pub entry: Entry,
}

#[derive(Clone, Debug)]
pub struct LaneSnapshot {
    pub lane: String,
    pub transcript: Vec<Entry>,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
    pub steer: Vec<QueuedItem>,
    pub follow_up: Vec<QueuedItem>,
    pub next_run: Vec<QueuedItem>,
    pub pending_writes: Vec<PendingWrite>,
    pub faulted: bool,
}

#[derive(Clone, Debug)]
pub struct SessionLaneSnapshot {
    pub lane: LaneInfo,
    pub suspended: Option<SuspendedOperation>,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub lanes: Vec<SessionLaneSnapshot>,
    pub faulted: bool,
}

pub type CreateLaneResult = Result<Arc<dyn AgentLane>, HarnessRejection>;

/// Port of `HookName`.
pub const HOOK_NAMES: &[&str] = &[
    "before_run",
    "before_resume",
    "before_run_end",
    "transform_context",
    "before_request",
    "before_payload",
    "after_response",
    "before_tool",
    "after_tool",
    "before_compaction",
    "before_navigation",
];

/// Port of `Resources`.
#[derive(Clone, Debug, Default)]
pub struct Resources {
    pub skills: Option<Vec<Skill>>,
    pub prompt_templates: Option<Vec<PromptTemplate>>,
}

/// Port of `AgentHarnessOptions`.
pub struct AgentHarnessOptions {
    pub session: Session,
    pub models: Arc<Models>,
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
    pub active_tool_names: Option<Vec<String>>,
    pub tools: Option<Vec<HarnessTool>>,
    pub tool_context: Option<ToolContextSource>,
    pub system_prompt: Option<SystemPromptSource>,
    pub stream_options: Option<SimpleStreamOptions>,
    pub retry: Option<RetryPolicy>,
    pub compaction: Option<CompactionSettings>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub tool_execution: Option<ToolExecution>,
    pub drive: Option<DriveMode>,
    pub to_provider_messages: Option<ToProviderMessages>,
    pub entry_projectors: Option<BTreeMap<String, EntryProjector>>,
    pub context: Option<Arc<dyn crate::telemetry::TelemetryContext>>,
    pub resources: Option<Resources>,
}

/// The public lane contract implemented by the root harness and named lanes.
pub trait AgentLane: Send + Sync {
    fn name(&self) -> &str;
    fn session(&self) -> &Session;
    fn get_leaf_id(
        &self,
    ) -> BoxFuture<'_, Result<Option<String>, crate::agent::harness::session::types::SessionError>>;
    fn prompt(&self, input: PromptInput) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>>;
    fn skill(
        &self,
        name: String,
        additional_instructions: Option<String>,
    ) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>>;
    fn prompt_from_template(
        &self,
        name: String,
        args: Option<Vec<String>>,
    ) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>>;
    fn compact(
        &self,
        options: Option<HarnessCompactOptions>,
    ) -> BoxFuture<'_, Result<CompactionResult, HarnessScaffoldError>>;
    fn navigate_tree(
        &self,
        target_id: Option<String>,
        options: Option<NavigateOptions>,
    ) -> BoxFuture<'_, Result<NavigationResult, HarnessScaffoldError>>;
    fn resume(
        &self,
    ) -> BoxFuture<'_, Result<Result<ResumeOutcome, HarnessRejection>, HarnessScaffoldError>>;
    fn abort(
        &self,
    ) -> BoxFuture<'_, Result<Result<AbortedRun, HarnessRejection>, HarnessScaffoldError>>;
    fn steer(&self, input: QueueInput) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>>;
    fn follow_up(
        &self,
        input: QueueInput,
    ) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>>;
    fn next_run(
        &self,
        input: QueueInput,
    ) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>>;
    fn cancel_queued(
        &self,
        entry_id: String,
    ) -> BoxFuture<'_, Result<Result<CancelQueuedOutcome, HarnessRejection>, HarnessScaffoldError>>;
    fn record_usage(
        &self,
        usage: Usage,
        options: Option<RecordUsageOptions>,
    ) -> BoxFuture<'_, Result<RecordUsageResult, HarnessScaffoldError>>;
    fn wait_for_idle(&self) -> BoxFuture<'_, Result<(), HarnessScaffoldError>>;
    fn run_when_idle(
        &self,
        callback: IdleCallback,
    ) -> BoxFuture<'_, Result<(), HarnessScaffoldError>>;
    fn peek_action(&self) -> BoxFuture<'_, Result<Option<ActionInfo>, HarnessScaffoldError>>;
    fn execute_action(&self) -> BoxFuture<'_, Result<Option<ActionInfo>, HarnessScaffoldError>>;
    fn run_to_completion(&self) -> BoxFuture<'_, Result<(), HarnessScaffoldError>>;
    fn get_model(&self) -> BoxFuture<'_, Model>;
    fn set_model(&self, model: Model) -> BoxFuture<'_, ()>;
    fn get_thinking_level(&self) -> BoxFuture<'_, ThinkingLevel>;
    fn set_thinking_level(&self, level: ThinkingLevel) -> BoxFuture<'_, ()>;
    fn get_active_tools(&self) -> BoxFuture<'_, Vec<String>>;
    fn set_active_tools(&self, names: Vec<String>) -> BoxFuture<'_, ()>;
    fn watch(
        &self,
    ) -> BoxFuture<
        '_,
        Result<crate::agent::harness::events::WatchHandle<LaneSnapshot>, HarnessScaffoldError>,
    >;
}

/// Port of the `AgentHarness` v2 scaffold.
pub struct AgentHarness {
    session: Session,
    model: std::sync::Mutex<Model>,
    thinking_level: std::sync::Mutex<ThinkingLevel>,
    active_tool_names: std::sync::Mutex<Vec<String>>,
    tools: std::sync::Mutex<Vec<HarnessTool>>,
    resources: std::sync::Mutex<Resources>,
    stream_options: std::sync::Mutex<SimpleStreamOptions>,
    retry_policy: std::sync::Mutex<RetryPolicy>,
    compaction_settings: std::sync::Mutex<CompactionSettings>,
    steering_mode: std::sync::Mutex<QueueMode>,
    follow_up_mode: std::sync::Mutex<QueueMode>,
    closed: std::sync::atomic::AtomicBool,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AgentHarness {
    /// The lane name this root harness mirrors.
    pub const NAME: &'static str = "main";

    /// Port of `AgentHarness.create`: opens only record-free sessions
    /// before restore is implemented.
    pub async fn create(
        options: AgentHarnessOptions,
    ) -> Result<(Arc<Self>, Vec<SuspendedOperation>), HarnessNotImplemented> {
        let records = options
            .session
            .find_records(Default::default())
            .await
            .unwrap_or_default();
        if !records.is_empty() {
            return Err(HarnessNotImplemented::new("create.restore"));
        }
        let tools = options.tools.unwrap_or_default();
        let active_tool_names = options
            .active_tool_names
            .unwrap_or_else(|| tools.iter().map(|tool| tool.name.clone()).collect());
        let harness = Arc::new(Self {
            session: options.session,
            model: std::sync::Mutex::new(options.model),
            thinking_level: std::sync::Mutex::new(
                options.thinking_level.unwrap_or(ThinkingLevel::Off),
            ),
            active_tool_names: std::sync::Mutex::new(active_tool_names),
            tools: std::sync::Mutex::new(tools),
            resources: std::sync::Mutex::new(options.resources.unwrap_or_default()),
            stream_options: std::sync::Mutex::new(options.stream_options.unwrap_or_default()),
            retry_policy: std::sync::Mutex::new(options.retry.unwrap_or(RetryPolicy {
                enabled: false,
                max_retries: 0,
                base_delay_ms: 1000,
            })),
            compaction_settings: std::sync::Mutex::new(
                options.compaction.unwrap_or(DEFAULT_COMPACTION_SETTINGS),
            ),
            steering_mode: std::sync::Mutex::new(
                options.steering_mode.unwrap_or(QueueMode::OneAtATime),
            ),
            follow_up_mode: std::sync::Mutex::new(
                options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
            ),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        Ok((harness, Vec::new()))
    }

    /// The durable session tree this harness drives.
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn unavailable<T>(&self, operation: &str) -> Result<T, HarnessScaffoldError> {
        if self.is_closed() {
            Err(HarnessScaffoldError::Closed(HarnessClosed))
        } else {
            Err(HarnessScaffoldError::NotImplemented(
                HarnessNotImplemented::new(operation),
            ))
        }
    }

    /// Whether `close` has been called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Port of `hooks.on` / `events.on`: always rejected by the scaffold.
    pub fn register_hook(
        &self,
        _name: &str,
        _handler: HookHandler,
        _options: Option<HookOptions>,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, HarnessScaffoldError> {
        self.unavailable("hooks.on")
    }

    /// The events face (also always rejected by the scaffold).
    pub fn register_event_listener(
        &self,
        _event_type: crate::agent::harness::events::HarnessEventType,
        _listener: crate::agent::harness::events::HarnessEventListener,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, HarnessScaffoldError> {
        self.unavailable("events.on")
    }

    /// Port of `getLeafId`.
    pub async fn get_leaf_id(
        &self,
    ) -> Result<Option<String>, crate::agent::harness::session::types::SessionError> {
        self.session.get_leaf_id().await
    }

    // Unimplemented operation surface.
    pub async fn prompt(&self, _input: PromptInput) -> Result<RunResult, HarnessScaffoldError> {
        self.unavailable("prompt")
    }
    pub async fn skill(
        &self,
        _name: &str,
        _additional_instructions: Option<&str>,
    ) -> Result<RunResult, HarnessScaffoldError> {
        self.unavailable("skill")
    }
    pub async fn prompt_from_template(
        &self,
        _name: &str,
        _args: Option<&[String]>,
    ) -> Result<RunResult, HarnessScaffoldError> {
        self.unavailable("promptFromTemplate")
    }
    pub async fn compact(
        &self,
        _options: Option<HarnessCompactOptions>,
    ) -> Result<CompactionResult, HarnessScaffoldError> {
        self.unavailable("compact")
    }
    pub async fn navigate_tree(
        &self,
        _target_id: Option<&str>,
        _options: Option<NavigateOptions>,
    ) -> Result<NavigationResult, HarnessScaffoldError> {
        self.unavailable("navigateTree")
    }
    pub async fn resume(
        &self,
    ) -> Result<Result<ResumeOutcome, HarnessRejection>, HarnessScaffoldError> {
        self.unavailable("resume")
    }
    pub async fn abort(
        &self,
    ) -> Result<Result<AbortedRun, HarnessRejection>, HarnessScaffoldError> {
        self.unavailable("abort")
    }
    pub async fn steer(&self, _input: QueueInput) -> Result<QueueResult, HarnessScaffoldError> {
        self.unavailable("steer")
    }
    pub async fn follow_up(&self, _input: QueueInput) -> Result<QueueResult, HarnessScaffoldError> {
        self.unavailable("followUp")
    }
    pub async fn next_run(&self, _input: QueueInput) -> Result<QueueResult, HarnessScaffoldError> {
        self.unavailable("nextRun")
    }
    pub async fn cancel_queued(
        &self,
        _entry_id: &str,
    ) -> Result<Result<CancelQueuedOutcome, HarnessRejection>, HarnessScaffoldError> {
        self.unavailable("cancelQueued")
    }
    pub async fn record_usage(
        &self,
        _usage: Usage,
        _options: Option<RecordUsageOptions>,
    ) -> Result<RecordUsageResult, HarnessScaffoldError> {
        self.unavailable("recordUsage")
    }
    pub async fn wait_for_idle(&self) -> Result<(), HarnessScaffoldError> {
        self.unavailable("waitForIdle")
    }
    pub async fn run_when_idle(&self, _callback: IdleCallback) -> Result<(), HarnessScaffoldError> {
        self.unavailable("runWhenIdle")
    }
    pub async fn peek_action(&self) -> Result<Option<ActionInfo>, HarnessScaffoldError> {
        self.unavailable("peekAction")
    }
    pub async fn execute_action(&self) -> Result<Option<ActionInfo>, HarnessScaffoldError> {
        self.unavailable("executeAction")
    }
    pub async fn run_to_completion(&self) -> Result<(), HarnessScaffoldError> {
        self.unavailable("runToCompletion")
    }
    pub async fn watch(
        &self,
    ) -> Result<crate::agent::harness::events::WatchHandle<LaneSnapshot>, HarnessScaffoldError>
    {
        self.unavailable("watch")
    }
    pub async fn lane(
        &self,
        _name: &str,
    ) -> Result<Option<Arc<dyn AgentLane>>, HarnessScaffoldError> {
        self.unavailable("lane")
    }
    pub async fn create_lane(
        &self,
        _name: &str,
        _at: Option<&str>,
    ) -> Result<CreateLaneResult, HarnessScaffoldError> {
        self.unavailable("createLane")
    }
    pub async fn lanes(&self) -> Result<Vec<LaneInfo>, HarnessScaffoldError> {
        self.unavailable("lanes")
    }
    pub async fn watch_session(
        &self,
    ) -> Result<crate::agent::harness::events::WatchHandle<SessionSnapshot>, HarnessScaffoldError>
    {
        self.unavailable("watchSession")
    }

    // Configuration surface (defensive copies like the TypeScript getter
    // and setter pairs).
    pub async fn get_model(&self) -> Model {
        lock(&self.model).clone()
    }
    pub async fn set_model(&self, model: Model) {
        *lock(&self.model) = model;
    }
    pub async fn get_thinking_level(&self) -> ThinkingLevel {
        *lock(&self.thinking_level)
    }
    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        *lock(&self.thinking_level) = level;
    }
    pub async fn get_active_tools(&self) -> Vec<String> {
        lock(&self.active_tool_names).clone()
    }
    pub async fn set_active_tools(&self, names: Vec<String>) {
        *lock(&self.active_tool_names) = names;
    }
    pub async fn get_tools(&self) -> Vec<HarnessTool> {
        lock(&self.tools).clone()
    }
    pub async fn set_tools(&self, tools: Vec<HarnessTool>, active_names: Option<Vec<String>>) {
        let names =
            active_names.unwrap_or_else(|| tools.iter().map(|tool| tool.name.clone()).collect());
        *lock(&self.tools) = tools;
        *lock(&self.active_tool_names) = names;
    }
    pub async fn get_resources(&self) -> Resources {
        lock(&self.resources).clone()
    }
    pub async fn set_resources(&self, resources: Resources) {
        *lock(&self.resources) = resources;
    }
    pub async fn get_stream_options(&self) -> SimpleStreamOptions {
        clone_stream_options(&lock(&self.stream_options))
    }
    pub async fn set_stream_options(&self, options: SimpleStreamOptions) {
        *lock(&self.stream_options) = clone_stream_options(&options);
    }
    pub async fn get_retry_policy(&self) -> RetryPolicy {
        *lock(&self.retry_policy)
    }
    pub async fn set_retry_policy(&self, policy: RetryPolicy) {
        *lock(&self.retry_policy) = policy;
    }
    pub async fn get_compaction_settings(&self) -> CompactionSettings {
        *lock(&self.compaction_settings)
    }
    pub async fn set_compaction_settings(&self, settings: CompactionSettings) {
        *lock(&self.compaction_settings) = settings;
    }
    pub async fn get_steering_mode(&self) -> QueueMode {
        *lock(&self.steering_mode)
    }
    pub async fn set_steering_mode(&self, mode: QueueMode) {
        *lock(&self.steering_mode) = mode;
    }
    pub async fn get_follow_up_mode(&self) -> QueueMode {
        *lock(&self.follow_up_mode)
    }
    pub async fn set_follow_up_mode(&self, mode: QueueMode) {
        *lock(&self.follow_up_mode) = mode;
    }

    /// Port of `close`.
    pub async fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Clones the stream options the way the TypeScript spread does.
fn clone_stream_options(options: &SimpleStreamOptions) -> SimpleStreamOptions {
    options.clone()
}

impl AgentLane for AgentHarness {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn session(&self) -> &Session {
        self.session()
    }

    fn get_leaf_id(
        &self,
    ) -> BoxFuture<'_, Result<Option<String>, crate::agent::harness::session::types::SessionError>>
    {
        Box::pin(self.get_leaf_id())
    }

    fn prompt(&self, input: PromptInput) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>> {
        Box::pin(self.prompt(input))
    }

    fn skill(
        &self,
        name: String,
        additional_instructions: Option<String>,
    ) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>> {
        Box::pin(async move { self.skill(&name, additional_instructions.as_deref()).await })
    }

    fn prompt_from_template(
        &self,
        name: String,
        args: Option<Vec<String>>,
    ) -> BoxFuture<'_, Result<RunResult, HarnessScaffoldError>> {
        Box::pin(async move { self.prompt_from_template(&name, args.as_deref()).await })
    }

    fn compact(
        &self,
        options: Option<HarnessCompactOptions>,
    ) -> BoxFuture<'_, Result<CompactionResult, HarnessScaffoldError>> {
        Box::pin(self.compact(options))
    }

    fn navigate_tree(
        &self,
        target_id: Option<String>,
        options: Option<NavigateOptions>,
    ) -> BoxFuture<'_, Result<NavigationResult, HarnessScaffoldError>> {
        Box::pin(async move { self.navigate_tree(target_id.as_deref(), options).await })
    }

    fn resume(
        &self,
    ) -> BoxFuture<'_, Result<Result<ResumeOutcome, HarnessRejection>, HarnessScaffoldError>> {
        Box::pin(self.resume())
    }

    fn abort(
        &self,
    ) -> BoxFuture<'_, Result<Result<AbortedRun, HarnessRejection>, HarnessScaffoldError>> {
        Box::pin(self.abort())
    }

    fn steer(&self, input: QueueInput) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>> {
        Box::pin(self.steer(input))
    }

    fn follow_up(
        &self,
        input: QueueInput,
    ) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>> {
        Box::pin(self.follow_up(input))
    }

    fn next_run(
        &self,
        input: QueueInput,
    ) -> BoxFuture<'_, Result<QueueResult, HarnessScaffoldError>> {
        Box::pin(self.next_run(input))
    }

    fn cancel_queued(
        &self,
        entry_id: String,
    ) -> BoxFuture<'_, Result<Result<CancelQueuedOutcome, HarnessRejection>, HarnessScaffoldError>>
    {
        Box::pin(async move { self.cancel_queued(&entry_id).await })
    }

    fn record_usage(
        &self,
        usage: Usage,
        options: Option<RecordUsageOptions>,
    ) -> BoxFuture<'_, Result<RecordUsageResult, HarnessScaffoldError>> {
        Box::pin(self.record_usage(usage, options))
    }

    fn wait_for_idle(&self) -> BoxFuture<'_, Result<(), HarnessScaffoldError>> {
        Box::pin(self.wait_for_idle())
    }

    fn run_when_idle(
        &self,
        callback: IdleCallback,
    ) -> BoxFuture<'_, Result<(), HarnessScaffoldError>> {
        Box::pin(self.run_when_idle(callback))
    }

    fn peek_action(&self) -> BoxFuture<'_, Result<Option<ActionInfo>, HarnessScaffoldError>> {
        Box::pin(self.peek_action())
    }

    fn execute_action(&self) -> BoxFuture<'_, Result<Option<ActionInfo>, HarnessScaffoldError>> {
        Box::pin(self.execute_action())
    }

    fn run_to_completion(&self) -> BoxFuture<'_, Result<(), HarnessScaffoldError>> {
        Box::pin(self.run_to_completion())
    }

    fn get_model(&self) -> BoxFuture<'_, Model> {
        Box::pin(self.get_model())
    }

    fn set_model(&self, model: Model) -> BoxFuture<'_, ()> {
        Box::pin(self.set_model(model))
    }

    fn get_thinking_level(&self) -> BoxFuture<'_, ThinkingLevel> {
        Box::pin(self.get_thinking_level())
    }

    fn set_thinking_level(&self, level: ThinkingLevel) -> BoxFuture<'_, ()> {
        Box::pin(self.set_thinking_level(level))
    }

    fn get_active_tools(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(self.get_active_tools())
    }

    fn set_active_tools(&self, names: Vec<String>) -> BoxFuture<'_, ()> {
        Box::pin(self.set_active_tools(names))
    }

    fn watch(
        &self,
    ) -> BoxFuture<
        '_,
        Result<crate::agent::harness::events::WatchHandle<LaneSnapshot>, HarnessScaffoldError>,
    > {
        Box::pin(self.watch())
    }
}

impl Hooks for AgentHarness {
    fn on(
        &self,
        name: &str,
        handler: HookHandler,
        options: Option<HookOptions>,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, HarnessScaffoldError> {
        self.register_hook(name, handler, options)
    }
}
