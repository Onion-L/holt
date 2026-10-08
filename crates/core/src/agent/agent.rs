//! Port of `pi-core/agent/src/agent.ts`.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::ai::types::{
    AssistantMessage, BlockContent, JsF64, Message, Model, SimpleStreamOptions, StopReason,
    TextContent, ThinkingBudgets, Transport, Usage, UsageCost,
};

use super::agent_loop::{AgentEventSink, run_agent_loop, run_agent_loop_continue};
use super::types::{
    AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate, AgentMessage,
    AgentState, AgentTool, BeforeToolCallFn, ConvertToLlmFn, GetApiKeyFn, GetMessagesFn,
    PrepareNextTurnContext, PrepareNextTurnFn, QueueMode, ShouldStopAfterTurnContext,
    ShouldStopAfterTurnFn, StreamFn, ThinkingLevel, ToolExecutionMode, TransformContextFn,
    now_millis,
};

const EMPTY_USAGE: Usage = Usage {
    input: 0,
    output: 0,
    cache_read: 0,
    cache_write: 0,
    total_tokens: 0,
    cache_write_1h: None,
    reasoning: None,
    cost: UsageCost {
        input: JsF64(0.0),
        output: JsF64(0.0),
        cache_read: JsF64(0.0),
        cache_write: JsF64(0.0),
        total: JsF64(0.0),
    },
};

fn default_model() -> Model {
    Model {
        id: "unknown".to_string(),
        name: "unknown".to_string(),
        api: "unknown".to_string(),
        provider: "unknown".to_string(),
        base_url: String::new(),
        reasoning: false,
        input: Vec::new(),
        context_window: 0,
        max_tokens: 0,
        ..Default::default()
    }
}

/// The agent-level `shouldStopAfterTurn` hook (`(context, signal)` flavor).
pub type AgentShouldStopAfterTurnFn = Arc<
    dyn Fn(ShouldStopAfterTurnContext, Option<CancellationToken>) -> BoxFuture<'static, bool>
        + Send
        + Sync,
>;

/// The agent-level legacy `prepareNextTurn` hook (`(signal)` flavor).
pub type LegacyPrepareNextTurnFn = Arc<
    dyn Fn(Option<CancellationToken>) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

/// The agent-level `prepareNextTurnWithContext` hook.
pub type PrepareNextTurnWithContextFn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            Option<CancellationToken>,
        ) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

/// Port of `AgentInitialState`: the subset of `AgentState` accepted by
/// `AgentOptions.initialState`.
#[derive(Default)]
pub struct AgentInitialState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Option<Vec<AgentTool>>,
    pub messages: Option<Vec<AgentMessage>>,
}

/// Port of `AgentOptions`.
#[derive(Default)]
pub struct AgentOptions {
    pub initial_state: Option<AgentInitialState>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub on_payload: Option<crate::ai::types::OnPayloadCallback>,
    pub on_response: Option<crate::ai::types::OnResponseCallback>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub should_stop_after_turn: Option<AgentShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<LegacyPrepareNextTurnFn>,
    pub prepare_next_turn_with_context: Option<PrepareNextTurnWithContextFn>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub transport: Option<Transport>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: Option<ToolExecutionMode>,
}

/// Prompt input accepted by [`Agent::prompt`] (the TypeScript string /
/// message / batch overloads).
pub enum AgentPromptInput {
    Text {
        text: String,
        images: Vec<crate::ai::types::ImageContent>,
    },
    Message(AgentMessage),
    Messages(Vec<AgentMessage>),
}

impl From<&str> for AgentPromptInput {
    fn from(text: &str) -> Self {
        AgentPromptInput::Text {
            text: text.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for AgentPromptInput {
    fn from(text: String) -> Self {
        AgentPromptInput::Text {
            text,
            images: Vec::new(),
        }
    }
}

impl From<AgentMessage> for AgentPromptInput {
    fn from(message: AgentMessage) -> Self {
        AgentPromptInput::Message(message)
    }
}

impl From<Vec<AgentMessage>> for AgentPromptInput {
    fn from(messages: Vec<AgentMessage>) -> Self {
        AgentPromptInput::Messages(messages)
    }
}

struct MutableAgentState {
    system_prompt: String,
    model: Model,
    thinking_level: ThinkingLevel,
    tools: Vec<AgentTool>,
    messages: Vec<AgentMessage>,
    is_streaming: bool,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: HashSet<String>,
    error_message: Option<String>,
}

impl Default for MutableAgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: default_model(),
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        }
    }
}

struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        if self.mode == QueueMode::All {
            return std::mem::take(&mut self.messages);
        }
        if self.messages.is_empty() {
            return Vec::new();
        }
        vec![self.messages.remove(0)]
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

struct ActiveRun {
    abort: CancellationToken,
}

struct AgentHooks {
    convert_to_llm: ConvertToLlmFn,
    transform_context: Option<TransformContextFn>,
    stream_function: Option<StreamFn>,
    get_api_key: Option<GetApiKeyFn>,
    on_payload: Option<crate::ai::types::OnPayloadCallback>,
    on_response: Option<crate::ai::types::OnResponseCallback>,
    before_tool_call: Option<BeforeToolCallFn>,
    after_tool_call: Option<AfterToolCallFn>,
    should_stop_after_turn: Option<AgentShouldStopAfterTurnFn>,
    prepare_next_turn: Option<LegacyPrepareNextTurnFn>,
    prepare_next_turn_with_context: Option<PrepareNextTurnWithContextFn>,
    session_id: Option<String>,
    thinking_budgets: Option<ThinkingBudgets>,
    transport: Transport,
    max_retry_delay_ms: Option<u64>,
    tool_execution: ToolExecutionMode,
}

/// Agent lifecycle listener (`Agent.subscribe`).
pub type AgentEventListener =
    Arc<dyn Fn(&AgentEvent, &CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

struct ListenerSet {
    next_id: AtomicU64,
    listeners: Mutex<Vec<(u64, AgentEventListener)>>,
}

/// Unsubscribe handle returned by [`Agent::subscribe`].
pub struct Subscription {
    listeners: Weak<ListenerSet>,
    id: u64,
}

impl Subscription {
    /// Removes the listener; safe to call more than once.
    pub fn unsubscribe(&self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners
                .listeners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(id, _)| *id != self.id);
        }
    }
}

/// Port of `Agent`: a stateful wrapper around the low-level agent loop.
///
/// `Agent` owns the current transcript, emits lifecycle events, executes
/// tools, and exposes queueing APIs for steering and follow-up messages.
/// All methods take `&self`; share the agent through an `Arc` to drive it
/// from multiple tasks.
pub struct Agent {
    inner: Arc<AgentInner>,
}

struct AgentInner {
    state: Mutex<MutableAgentState>,
    listeners: Arc<ListenerSet>,
    steering_queue: Mutex<PendingMessageQueue>,
    follow_up_queue: Mutex<PendingMessageQueue>,
    hooks: Mutex<AgentHooks>,
    active_run: Mutex<Option<ActiveRun>>,
    run_lock: Arc<tokio::sync::Mutex<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Agent {
    /// Port of the `Agent` constructor.
    pub fn new(options: AgentOptions) -> Self {
        let initial_state = options.initial_state.unwrap_or_default();
        let state = MutableAgentState {
            system_prompt: initial_state.system_prompt.unwrap_or_default(),
            model: initial_state.model.unwrap_or_else(default_model),
            thinking_level: initial_state.thinking_level.unwrap_or(ThinkingLevel::Off),
            tools: initial_state.tools.unwrap_or_default(),
            messages: initial_state.messages.unwrap_or_default(),
            ..Default::default()
        };
        let convert_to_llm = options.convert_to_llm.unwrap_or_else(|| {
            Arc::new(|messages: Vec<AgentMessage>| {
                Box::pin(async move { super::agent_loop::pass_through_llm_messages(messages) })
                    as BoxFuture<'static, Vec<Message>>
            })
        });
        Self {
            inner: Arc::new(AgentInner {
                state: Mutex::new(state),
                listeners: Arc::new(ListenerSet {
                    next_id: AtomicU64::new(0),
                    listeners: Mutex::new(Vec::new()),
                }),
                steering_queue: Mutex::new(PendingMessageQueue::new(
                    options.steering_mode.unwrap_or(QueueMode::OneAtATime),
                )),
                follow_up_queue: Mutex::new(PendingMessageQueue::new(
                    options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
                )),
                hooks: Mutex::new(AgentHooks {
                    convert_to_llm,
                    transform_context: options.transform_context,
                    stream_function: options.stream_fn,
                    get_api_key: options.get_api_key,
                    on_payload: options.on_payload,
                    on_response: options.on_response,
                    before_tool_call: options.before_tool_call,
                    after_tool_call: options.after_tool_call,
                    should_stop_after_turn: options.should_stop_after_turn,
                    prepare_next_turn: options.prepare_next_turn,
                    prepare_next_turn_with_context: options.prepare_next_turn_with_context,
                    session_id: options.session_id,
                    thinking_budgets: options.thinking_budgets,
                    transport: options.transport.unwrap_or(Transport::Auto),
                    max_retry_delay_ms: options.max_retry_delay_ms,
                    tool_execution: options
                        .tool_execution
                        .unwrap_or(ToolExecutionMode::Parallel),
                }),
                active_run: Mutex::new(None),
                run_lock: Arc::new(tokio::sync::Mutex::new(())),
            }),
        }
    }

    /// Subscribe to agent lifecycle events. Listener futures are awaited in
    /// subscription order and are included in the current run's settlement.
    pub fn subscribe(&self, listener: AgentEventListener) -> Subscription {
        let id = self.inner.listeners.next_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.listeners.listeners).push((id, listener));
        Subscription {
            listeners: Arc::downgrade(&self.inner.listeners),
            id,
        }
    }

    // -----------------------------------------------------------------------
    // State accessors (the `AgentState` surface)
    // -----------------------------------------------------------------------

    /// Captures all public state fields under one lock.
    pub fn state(&self) -> AgentState {
        let state = lock(&self.inner.state);
        AgentState {
            system_prompt: state.system_prompt.clone(),
            model: state.model.clone(),
            thinking_level: state.thinking_level,
            tools: state.tools.clone(),
            messages: state.messages.clone(),
            is_streaming: state.is_streaming,
            streaming_message: state.streaming_message.clone(),
            pending_tool_calls: state.pending_tool_calls.clone(),
            error_message: state.error_message.clone(),
        }
    }

    /// Current system prompt.
    pub fn system_prompt(&self) -> String {
        lock(&self.inner.state).system_prompt.clone()
    }

    /// Replaces the system prompt.
    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) {
        lock(&self.inner.state).system_prompt = system_prompt.into();
    }

    /// Active model used for future turns.
    pub fn model(&self) -> Model {
        lock(&self.inner.state).model.clone()
    }

    /// Replaces the active model.
    pub fn set_model(&self, model: Model) {
        lock(&self.inner.state).model = model;
    }

    /// Requested reasoning level for future turns.
    pub fn thinking_level(&self) -> ThinkingLevel {
        lock(&self.inner.state).thinking_level
    }

    /// Replaces the reasoning level.
    pub fn set_thinking_level(&self, thinking_level: ThinkingLevel) {
        lock(&self.inner.state).thinking_level = thinking_level;
    }

    /// Available tools (top-level array copy, like the TypeScript accessor).
    pub fn tools(&self) -> Vec<AgentTool> {
        lock(&self.inner.state).tools.clone()
    }

    /// Replaces the available tools (copies the top-level array).
    pub fn set_tools(&self, tools: Vec<AgentTool>) {
        lock(&self.inner.state).tools = tools;
    }

    /// Conversation transcript (top-level array copy).
    pub fn messages(&self) -> Vec<AgentMessage> {
        lock(&self.inner.state).messages.clone()
    }

    /// Replaces the transcript (copies the top-level array).
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        lock(&self.inner.state).messages = messages;
    }

    /// Appends a message to the transcript (`agent.state.messages.push`).
    pub fn push_message(&self, message: AgentMessage) {
        lock(&self.inner.state).messages.push(message);
    }

    /// True while the agent is processing a prompt or continuation.
    pub fn is_streaming(&self) -> bool {
        lock(&self.inner.state).is_streaming
    }

    /// Partial assistant message for the current streamed response, if any.
    pub fn streaming_message(&self) -> Option<AgentMessage> {
        lock(&self.inner.state).streaming_message.clone()
    }

    /// Tool call ids currently executing.
    pub fn pending_tool_calls(&self) -> HashSet<String> {
        lock(&self.inner.state).pending_tool_calls.clone()
    }

    /// Error message from the most recent failed or aborted turn, if any.
    pub fn error_message(&self) -> Option<String> {
        lock(&self.inner.state).error_message.clone()
    }

    // -----------------------------------------------------------------------
    // Queue modes and message queues
    // -----------------------------------------------------------------------

    /// Controls how queued steering messages are drained.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.inner.steering_queue).mode = mode;
    }

    /// Current steering drain mode.
    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.inner.steering_queue).mode
    }

    /// Controls how queued follow-up messages are drained.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.inner.follow_up_queue).mode = mode;
    }

    /// Current follow-up drain mode.
    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.inner.follow_up_queue).mode
    }

    /// Queue a message to be injected after the current assistant turn.
    pub fn steer(&self, message: AgentMessage) {
        lock(&self.inner.steering_queue).enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        lock(&self.inner.follow_up_queue).enqueue(message);
    }

    /// Remove all queued steering messages.
    pub fn clear_steering_queue(&self) {
        lock(&self.inner.steering_queue).clear();
    }

    /// Remove all queued follow-up messages.
    pub fn clear_follow_up_queue(&self) {
        lock(&self.inner.follow_up_queue).clear();
    }

    /// Remove all queued steering and follow-up messages.
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Returns true when either queue still contains pending messages.
    pub fn has_queued_messages(&self) -> bool {
        let steering = lock(&self.inner.steering_queue).has_items();
        let follow_up = lock(&self.inner.follow_up_queue).has_items();
        steering || follow_up
    }

    // -----------------------------------------------------------------------
    // Run control
    // -----------------------------------------------------------------------

    /// Active abort signal for the current run, if any.
    pub fn signal(&self) -> Option<CancellationToken> {
        lock(&self.inner.active_run)
            .as_ref()
            .map(|run| run.abort.clone())
    }

    /// Abort the current run, if one is active.
    pub fn abort(&self) {
        if let Some(run) = lock(&self.inner.active_run).as_ref() {
            run.abort.cancel();
        }
    }

    /// Resolves when the current run and all awaited event listeners have
    /// finished (after `agent_end` listeners settle).
    pub async fn wait_for_idle(&self) {
        let guard = Arc::clone(&self.inner.run_lock).lock_owned().await;
        drop(guard);
    }

    /// Clear transcript state, runtime state, and queued messages.
    pub fn reset(&self) -> Result<(), String> {
        if lock(&self.inner.active_run).is_some() {
            return Err(
                "Agent is already processing. Wait for completion before resetting.".to_string(),
            );
        }
        {
            let mut state = lock(&self.inner.state);
            state.messages.clear();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
            state.error_message = None;
        }
        self.clear_follow_up_queue();
        self.clear_steering_queue();
        Ok(())
    }

    /// Start a new prompt from text, a single message, or a batch.
    pub async fn prompt(&self, input: impl Into<AgentPromptInput>) -> Result<(), String> {
        if self.has_active_run() {
            return Err(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
                    .to_string(),
            );
        }
        let messages = self.normalize_prompt_input(input.into());
        self.run_prompt_messages(messages, false).await
    }

    /// Continue from the current transcript. The last message must be a
    /// user or tool-result message.
    pub async fn continue_(&self) -> Result<(), String> {
        if self.has_active_run() {
            return Err(
                "Agent is already processing. Wait for completion before continuing.".to_string(),
            );
        }

        let last_message = self.messages().pop();
        let Some(last_message) = last_message else {
            return Err("No messages to continue from".to_string());
        };

        if last_message.role() == "assistant" {
            let queued_steering = lock(&self.inner.steering_queue).drain();
            if !queued_steering.is_empty() {
                return self.run_prompt_messages(queued_steering, true).await;
            }

            let queued_follow_ups = lock(&self.inner.follow_up_queue).drain();
            if !queued_follow_ups.is_empty() {
                return self.run_prompt_messages(queued_follow_ups, false).await;
            }

            return Err("Cannot continue from message role: assistant".to_string());
        }

        self.run_continuation().await
    }

    // -----------------------------------------------------------------------
    // Hook accessors (public mutable fields on the TypeScript Agent)
    // -----------------------------------------------------------------------

    /// Replaces `convertToLlm`.
    pub fn set_convert_to_llm(&self, convert_to_llm: ConvertToLlmFn) {
        lock(&self.inner.hooks).convert_to_llm = convert_to_llm;
    }

    /// Replaces `streamFn`.
    pub fn set_stream_fn(&self, stream_fn: StreamFn) {
        lock(&self.inner.hooks).stream_function = Some(stream_fn);
    }

    /// Session identifier forwarded to providers for cache-aware backends.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.inner.hooks).session_id.clone()
    }

    /// Replaces the session identifier.
    pub fn set_session_id(&self, session_id: Option<String>) {
        lock(&self.inner.hooks).session_id = session_id;
    }

    fn has_active_run(&self) -> bool {
        lock(&self.inner.active_run).is_some()
    }

    fn normalize_prompt_input(&self, input: AgentPromptInput) -> Vec<AgentMessage> {
        match input {
            AgentPromptInput::Messages(messages) => messages,
            AgentPromptInput::Message(message) => vec![message],
            AgentPromptInput::Text { text, images } => {
                let mut content: Vec<BlockContent> = vec![BlockContent::Text(TextContent {
                    text,
                    ..Default::default()
                })];
                for image in images {
                    content.push(BlockContent::Image(image));
                }
                vec![AgentMessage::User(crate::ai::types::UserMessage {
                    role: Default::default(),
                    content: crate::ai::types::UserContent::Blocks(content),
                    timestamp: now_millis(),
                })]
            }
        }
    }

    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    ) -> Result<(), String> {
        self.run_with_lifecycle(|signal| async move {
            let context = self.create_context_snapshot();
            let config = self.create_loop_config(signal.clone(), skip_initial_steering_poll);
            let stream_fn = lock(&self.inner.hooks).stream_function.clone();
            run_agent_loop(
                messages,
                context,
                config,
                self.emit_sink(),
                Some(signal),
                stream_fn,
            )
            .await
            .map(|_| ())
        })
        .await
    }

    async fn run_continuation(&self) -> Result<(), String> {
        self.run_with_lifecycle(|signal| async move {
            let context = self.create_context_snapshot();
            let config = self.create_loop_config(signal.clone(), false);
            let stream_fn = lock(&self.inner.hooks).stream_function.clone();
            run_agent_loop_continue(context, config, self.emit_sink(), Some(signal), stream_fn)
                .await
                .map(|_| ())
        })
        .await
    }

    fn emit_sink(&self) -> AgentEventSink {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |event: AgentEvent| {
            let inner = inner.clone();
            Box::pin(async move {
                let inner = inner.upgrade().expect("agent outlives its runs");
                process_events(&inner, event)
                    .await
                    .expect("agent listener invoked outside active run");
            }) as BoxFuture<'static, ()>
        })
    }

    fn create_context_snapshot(&self) -> AgentContext {
        let state = lock(&self.inner.state);
        AgentContext {
            system_prompt: state.system_prompt.clone(),
            messages: state.messages.clone(),
            tools: Some(state.tools.clone()),
        }
    }

    fn create_loop_config(
        &self,
        signal: CancellationToken,
        skip_initial_steering_poll: bool,
    ) -> AgentLoopConfig {
        let hooks = lock(&self.inner.hooks);
        let thinking_level = lock(&self.inner.state).thinking_level;
        let should_stop_after_turn = hooks.should_stop_after_turn.clone();
        let prepare_next_turn = hooks.prepare_next_turn.clone();
        let prepare_next_turn_with_context = hooks.prepare_next_turn_with_context.clone();

        let skip_initial_poll = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        let steering_get: GetMessagesFn = {
            let skip_initial_poll = Arc::clone(&skip_initial_poll);
            let inner = Arc::downgrade(&self.inner);
            Arc::new(move || {
                if skip_initial_poll.swap(false, Ordering::SeqCst) {
                    return Box::pin(async move { Vec::new() });
                }
                let drained = inner
                    .upgrade()
                    .map(|inner| lock(&inner.steering_queue).drain())
                    .unwrap_or_default();
                Box::pin(async move { drained })
            })
        };
        let follow_up_get: GetMessagesFn = {
            let inner = Arc::downgrade(&self.inner);
            Arc::new(move || {
                let drained = inner
                    .upgrade()
                    .map(|inner| lock(&inner.follow_up_queue).drain())
                    .unwrap_or_default();
                Box::pin(async move { drained })
            })
        };

        AgentLoopConfig {
            stream_options: SimpleStreamOptions {
                base: crate::ai::types::StreamOptions {
                    base: crate::ai::types::ProviderRequestOptions {
                        on_payload: hooks.on_payload.clone(),
                        on_response: hooks.on_response.clone(),
                        max_retry_delay_ms: hooks.max_retry_delay_ms,
                        ..Default::default()
                    },
                    session_id: hooks.session_id.clone(),
                    transport: Some(hooks.transport),
                    ..Default::default()
                },
                reasoning: thinking_level.as_provider_reasoning(),
                thinking_budgets: hooks.thinking_budgets.clone(),
                ..Default::default()
            },
            model: lock(&self.inner.state).model.clone(),
            convert_to_llm: hooks.convert_to_llm.clone(),
            transform_context: hooks.transform_context.clone(),
            get_api_key: hooks.get_api_key.clone(),
            should_stop_after_turn: should_stop_after_turn.map(|should_stop| {
                let signal = signal.clone();
                Arc::new(move |context| {
                    let signal = signal.clone();
                    let should_stop = Arc::clone(&should_stop);
                    Box::pin(async move { should_stop(context, Some(signal)).await })
                        as BoxFuture<'static, bool>
                }) as ShouldStopAfterTurnFn
            }),
            prepare_next_turn: (prepare_next_turn.is_some()
                || prepare_next_turn_with_context.is_some())
            .then(|| {
                let signal = signal.clone();
                let with_context = prepare_next_turn_with_context.clone();
                let legacy = prepare_next_turn.clone();
                Arc::new(move |context: PrepareNextTurnContext| {
                    let signal = signal.clone();
                    let with_context = with_context.clone();
                    let legacy = legacy.clone();
                    Box::pin(async move {
                        if let Some(with_context) = with_context {
                            return with_context(context, Some(signal)).await;
                        }
                        legacy.expect("checked at construction")(Some(signal)).await
                    }) as BoxFuture<'static, Option<AgentLoopTurnUpdate>>
                }) as PrepareNextTurnFn
            }),
            get_steering_messages: Some(steering_get),
            get_follow_up_messages: Some(follow_up_get),
            tool_execution: Some(hooks.tool_execution),
            before_tool_call: hooks.before_tool_call.clone(),
            after_tool_call: hooks.after_tool_call.clone(),
        }
    }

    async fn run_with_lifecycle<F, Fut>(&self, executor: F) -> Result<(), String>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        if self.has_active_run() {
            return Err("Agent is already processing.".to_string());
        }

        let guard = Arc::clone(&self.inner.run_lock).lock_owned().await;
        let abort = CancellationToken::new();
        *lock(&self.inner.active_run) = Some(ActiveRun {
            abort: abort.clone(),
        });
        {
            let mut state = lock(&self.inner.state);
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        }

        let result = executor(abort.clone()).await;
        let outcome = match result {
            Ok(()) => Ok(()),
            Err(error) => self.handle_run_failure(error, abort.is_cancelled()).await,
        };
        self.finish_run();
        drop(guard);
        outcome
    }

    async fn handle_run_failure(&self, error: String, aborted: bool) -> Result<(), String> {
        let (api, provider, model) = {
            let state = lock(&self.inner.state);
            (
                state.model.api.clone(),
                state.model.provider.clone(),
                state.model.id.clone(),
            )
        };
        let failure_message = AssistantMessage {
            role: Default::default(),
            content: vec![crate::ai::types::AssistantContent::Text(TextContent {
                text: String::new(),
                ..Default::default()
            })],
            api,
            provider,
            model,
            usage: EMPTY_USAGE,
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(error),
            timestamp: now_millis(),
            ..Default::default()
        };
        let failure = AgentMessage::Assistant(Box::new(failure_message));
        process_events(
            &self.inner,
            AgentEvent::MessageStart {
                message: Box::new(failure.clone()),
            },
        )
        .await?;
        process_events(
            &self.inner,
            AgentEvent::MessageEnd {
                message: Box::new(failure.clone()),
            },
        )
        .await?;
        process_events(
            &self.inner,
            AgentEvent::TurnEnd {
                message: Box::new(failure.clone()),
                tool_results: Vec::new(),
            },
        )
        .await?;
        process_events(
            &self.inner,
            AgentEvent::AgentEnd {
                messages: vec![failure],
            },
        )
        .await?;
        Ok(())
    }

    fn finish_run(&self) {
        {
            let mut state = lock(&self.inner.state);
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
        }
        *lock(&self.inner.active_run) = None;
    }
}

/// Reduce agent state for a loop event, then await listeners.
async fn process_events(inner: &Arc<AgentInner>, event: AgentEvent) -> Result<(), String> {
    match &event {
        AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
            lock(&inner.state).streaming_message = Some((**message).clone());
        }
        AgentEvent::MessageEnd { message } => {
            let mut state = lock(&inner.state);
            state.streaming_message = None;
            state.messages.push((**message).clone());
        }
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
            lock(&inner.state)
                .pending_tool_calls
                .insert(tool_call_id.clone());
        }
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
            lock(&inner.state).pending_tool_calls.remove(tool_call_id);
        }
        AgentEvent::TurnEnd { message, .. } => {
            if let AgentMessage::Assistant(assistant) = &**message
                && let Some(error_message) = &assistant.error_message
            {
                lock(&inner.state).error_message = Some(error_message.clone());
            }
        }
        AgentEvent::AgentEnd { .. } => {
            lock(&inner.state).streaming_message = None;
        }
        AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::ToolExecutionUpdate { .. } => {
        }
    }

    let signal = lock(&inner.active_run)
        .as_ref()
        .map(|run| run.abort.clone());
    let Some(signal) = signal else {
        return Err("Agent listener invoked outside active run".to_string());
    };
    let listeners = lock(&inner.listeners.listeners).clone();
    for (_, listener) in listeners {
        listener(&event, &signal).await;
    }
    Ok(())
}
