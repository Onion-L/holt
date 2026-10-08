//! Port of the WebSocket transport half of
//! `pi-core/ai/src/api/openai-codex-responses.ts`: the `WebSocketLike`
//! runtime seam, the account-scoped session cache with idle/age expiry and
//! continuation reuse, the SSE-fallback bookkeeping, and the debug-stats
//! registry.
//!
//! TypeScript reads the runtime's global `WebSocket` constructor; the Rust
//! port routes through a [`WebSocketFactory`] whose default implementation
//! is `tokio-tungstenite` and which tests replace with canned sockets (the
//! equivalent of `vi.stubGlobal("WebSocket", MockWebSocket)`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

use crate::ai::session_resources::register_session_resource_cleanup;

const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const SESSION_WEBSOCKET_MAX_AGE_MS: i64 = 55 * 60 * 1000;
pub(crate) const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;
pub(crate) const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str =
    "websocket_connection_limit_reached";
pub(crate) const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";

// ---------------------------------------------------------------------------
// Clock seam
// ---------------------------------------------------------------------------

type WsClock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn clock_override() -> &'static RwLock<Option<WsClock>> {
    static CLOCK: OnceLock<RwLock<Option<WsClock>>> = OnceLock::new();
    CLOCK.get_or_init(|| RwLock::new(None))
}

/// The WebSocket session cache's clock. Tests freeze it the way the
/// TypeScript suite freezes `Date.now` with `vi.setSystemTime`; production
/// reads the wall clock.
pub(crate) fn ws_now_ms() -> i64 {
    if let Some(clock) = clock_override()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        return clock();
    }
    crate::ai::auth::resolve::now_millis()
}

/// Installs the test clock (the `vi.setSystemTime` analog). Passing `None`
/// restores the wall clock.
pub fn set_websocket_clock_for_tests(clock: Option<WsClock>) {
    *clock_override()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = clock;
}

// ---------------------------------------------------------------------------
// The WebSocketLike seam
// ---------------------------------------------------------------------------

/// A socket event, the Rust shape of the DOM events the TypeScript adapter
/// listens for.
#[derive(Clone, Debug, PartialEq)]
pub enum WebSocketEvent {
    Open,
    Message(String),
    Error(String),
    Close {
        code: Option<u16>,
        reason: Option<String>,
    },
}

/// A FIFO of socket events shared between the socket implementation and the
/// adapter's single sequential consumer. Events buffer until drained, so a
/// producer may push before the consumer subscribes (the ordering the
/// TypeScript mocks establish with `queueMicrotask`).
#[derive(Clone, Default)]
pub struct WebSocketEventQueue {
    inner: Arc<Mutex<QueueInner>>,
    notify: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct QueueInner {
    events: VecDeque<WebSocketEvent>,
    finished: bool,
}

impl WebSocketEventQueue {
    pub fn push(&self, event: WebSocketEvent) {
        {
            let mut inner = self.lock();
            inner.events.push_back(event);
        }
        self.notify.notify_waiters();
    }

    /// Marks the producer finished; `next` returns `None` once drained.
    pub fn finish(&self) {
        self.lock().finished = true;
        self.notify.notify_waiters();
    }

    /// Waits for and removes the next event (`None` once finished and
    /// drained).
    pub async fn next(&self) -> Option<WebSocketEvent> {
        let notify = Arc::clone(&self.notify);
        loop {
            // Register before checking so a push racing the check still
            // wakes this waiter.
            let notified = notify.notified();
            {
                let mut inner = self.lock();
                if let Some(event) = inner.events.pop_front() {
                    return Some(event);
                }
                if inner.finished {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Port of the `WebSocketLike` interface.
pub trait WebSocketLike: Send + Sync {
    /// Port of `send` (the adapter sends a single JSON text frame).
    fn send_text(&self, data: &str);

    /// Port of `close(code, reason)` — best effort, errors swallowed.
    fn close_silently(&self, code: u16, reason: &str);

    /// Port of `isWebSocketReusable` (`readyState === OPEN`; runtimes that
    /// cannot observe the state stay reusable).
    fn is_reusable(&self) -> bool {
        true
    }

    /// Waits for the next buffered socket event. Events emitted before any
    /// consumer attaches are preserved in order.
    fn next_event(&self) -> BoxFuture<'static, Option<WebSocketEvent>>;
}

/// Port of the `WebSocketConstructor` lookup: builds a connected socket for
/// a URL with request headers. The default resolves after the handshake; a
/// test factory stands in for the mocked global constructor.
pub type WebSocketFactory = Arc<
    dyn Fn(
            String,
            Vec<(String, String)>,
        ) -> BoxFuture<'static, Result<Arc<dyn WebSocketLike>, String>>
        + Send
        + Sync,
>;

fn factory_override() -> &'static RwLock<Option<WebSocketFactory>> {
    static FACTORY: OnceLock<RwLock<Option<WebSocketFactory>>> = OnceLock::new();
    FACTORY.get_or_init(|| RwLock::new(None))
}

/// Replaces the WebSocket constructor for tests (`vi.stubGlobal` analog);
/// `None` restores the production connector.
pub fn set_websocket_factory_for_tests(factory: Option<WebSocketFactory>) {
    *factory_override()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = factory;
}

fn websocket_factory() -> WebSocketFactory {
    if let Some(factory) = factory_override()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return factory;
    }
    default_websocket_factory()
}

// ---------------------------------------------------------------------------
// Failure taxonomy (CodexApiError / CodexProtocolError / transport errors)
// ---------------------------------------------------------------------------

/// The error kinds driving the WebSocket retry/fallback decision, mirroring
/// `CodexApiError`, `CodexProtocolError`, and plain transport errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodexWsFailureKind {
    Api,
    Protocol,
    Transport,
}

/// A WebSocket-transport failure carrying the error taxonomy the TypeScript
/// adapter reads off `error instanceof CodexApiError` and `error.code`.
#[derive(Clone, Debug)]
pub(crate) struct CodexWsFailure {
    pub kind: CodexWsFailureKind,
    pub message: String,
    pub code: Option<String>,
}

impl CodexWsFailure {
    pub(crate) fn transport(message: impl Into<String>) -> Self {
        CodexWsFailure {
            kind: CodexWsFailureKind::Transport,
            message: message.into(),
            code: None,
        }
    }

    pub(crate) fn api(message: impl Into<String>, code: Option<String>) -> Self {
        CodexWsFailure {
            kind: CodexWsFailureKind::Api,
            message: message.into(),
            code,
        }
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        CodexWsFailure {
            kind: CodexWsFailureKind::Protocol,
            message: message.into(),
            code: None,
        }
    }

    /// Port of `isCodexNonTransportError`.
    pub fn is_non_transport(&self) -> bool {
        !matches!(self.kind, CodexWsFailureKind::Transport)
    }

    /// Port of `isWebSocketConnectionLimitReachedError`.
    pub fn is_connection_limit_reached(&self) -> bool {
        self.kind == CodexWsFailureKind::Api
            && self.code.as_deref() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
    }

    /// Port of `isPreviousResponseNotFoundError`.
    pub fn is_previous_response_not_found(&self) -> bool {
        self.kind == CodexWsFailureKind::Api
            && self.code.as_deref() == Some(PREVIOUS_RESPONSE_NOT_FOUND_CODE)
    }
}

impl std::fmt::Display for CodexWsFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CodexWsFailure {}

/// Port of `extractWebSocketCloseError`'s message rendering.
pub(crate) fn close_error_message(code: Option<u16>, reason: Option<&str>) -> String {
    let code_text = code.map(|code| format!(" {code}")).unwrap_or_default();
    let mut reason_text = reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| format!(" {reason}"))
        .unwrap_or_default();
    if reason_text.is_empty() && code == Some(WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE) {
        reason_text = " message too big".to_string();
    }
    format!("WebSocket closed{code_text}{reason_text}")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Debug stats, fallback set, and the session cache
// ---------------------------------------------------------------------------

/// Port of `OpenAICodexWebSocketDebugStats`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenAICodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: u64,
    pub last_delta_input_items: Option<u64>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

struct CachedWebSocketContinuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Value,
}

struct CachedWebSocketConnection {
    socket: Arc<dyn WebSocketLike>,
    busy: bool,
    created_at: i64,
    idle_timer: Option<tokio::task::JoinHandle<()>>,
    continuation: Option<CachedWebSocketContinuation>,
}

#[derive(Default)]
struct WsGlobalState {
    session_cache: HashMap<String, HashMap<String, CachedWebSocketConnection>>,
    debug_stats: HashMap<String, OpenAICodexWebSocketDebugStats>,
    sse_fallback_sessions: HashSet<String>,
}

fn ws_state() -> &'static Mutex<WsGlobalState> {
    static STATE: OnceLock<Mutex<WsGlobalState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(WsGlobalState::default()))
}

/// Port of `getOpenAICodexWebSocketDebugStats`.
pub fn get_openai_codex_websocket_debug_stats(
    session_id: &str,
) -> Option<OpenAICodexWebSocketDebugStats> {
    ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .debug_stats
        .get(session_id)
        .cloned()
}

/// Port of `resetOpenAICodexWebSocketDebugStats`.
pub fn reset_openai_codex_websocket_debug_stats(session_id: Option<&str>) {
    let mut state = ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match session_id {
        Some(session_id) => {
            state.debug_stats.remove(session_id);
            state.sse_fallback_sessions.remove(session_id);
        }
        None => {
            state.debug_stats.clear();
            state.sse_fallback_sessions.clear();
        }
    }
}

/// Port of `closeOpenAICodexWebSocketSessions`.
pub fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let mut state = ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let close_entry = |entry: &mut CachedWebSocketConnection| {
        if let Some(timer) = entry.idle_timer.take() {
            timer.abort();
        }
        entry.socket.close_silently(1000, "debug_close");
    };
    match session_id {
        Some(session_id) => {
            if let Some(accounts) = state.session_cache.get_mut(session_id) {
                for entry in accounts.values_mut() {
                    close_entry(entry);
                }
            }
            state.session_cache.remove(session_id);
        }
        None => {
            for accounts in state.session_cache.values_mut() {
                for entry in accounts.values_mut() {
                    close_entry(entry);
                }
            }
            state.session_cache.clear();
        }
    }
}

// The module-level registration mirrors the top-level
// `registerSessionResourceCleanup(closeOpenAICodexWebSocketSessions)` call.
fn register_cleanup_once() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        // The TypeScript registration is permanent (module scope); the
        // returned unregister closure is deliberately dropped.
        let _unregister =
            register_session_resource_cleanup(Arc::new(close_openai_codex_websocket_sessions));
    });
}

/// Port of `isWebSocketSseFallbackActive`.
pub(crate) fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    session_id.is_some_and(|session_id| {
        ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sse_fallback_sessions
            .contains(session_id)
    })
}

/// Port of `recordWebSocketSseFallback`.
pub(crate) fn record_websocket_sse_fallback(session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut state = ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fallback_active = state.sse_fallback_sessions.contains(session_id);
    let stats = state.debug_stats.entry(session_id.to_string()).or_default();
    stats.sse_fallbacks += 1;
    stats.websocket_fallback_active = Some(fallback_active);
}

/// Port of `recordWebSocketFailure`.
pub(crate) fn record_websocket_failure(session_id: Option<&str>, error: &CodexWsFailure) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut state = ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.sse_fallback_sessions.insert(session_id.to_string());
    let stats = state.debug_stats.entry(session_id.to_string()).or_default();
    stats.websocket_failures += 1;
    stats.last_websocket_error = Some(error.to_string());
    stats.websocket_fallback_active = Some(true);
}

pub(crate) fn get_or_create_websocket_debug_stats(
    session_id: &str,
) -> OpenAICodexWebSocketDebugStats {
    ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .debug_stats
        .entry(session_id.to_string())
        .or_default()
        .clone()
}

pub(crate) fn write_websocket_debug_stats(session_id: &str, stats: OpenAICodexWebSocketDebugStats) {
    ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .debug_stats
        .insert(session_id.to_string(), stats);
}

fn is_websocket_session_expired(entry: &CachedWebSocketConnection) -> bool {
    ws_now_ms() - entry.created_at >= SESSION_WEBSOCKET_MAX_AGE_MS
}

/// Port of `scheduleSessionWebSocketExpiry` (runs outside the state lock).
fn schedule_session_websocket_expiry(session_id: &str, account_id: &str) {
    let handle = tokio::spawn({
        let session_id = session_id.to_string();
        let account_id = account_id.to_string();
        async move {
            tokio::time::sleep(Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS)).await;
            let mut state = ws_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = state
                .session_cache
                .get_mut(&session_id)
                .and_then(|accounts| accounts.get_mut(&account_id))
            else {
                return;
            };
            if let Some(timer) = entry.idle_timer.take() {
                timer.abort();
            }
            if entry.busy {
                return;
            }
            entry.socket.close_silently(1000, "idle_timeout");
            if let Some(accounts) = state.session_cache.get_mut(&session_id) {
                accounts.remove(&account_id);
                if accounts.is_empty() {
                    state.session_cache.remove(&session_id);
                }
            }
        }
    });
    let mut state = ws_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = state
        .session_cache
        .get_mut(session_id)
        .and_then(|accounts| accounts.get_mut(account_id))
    {
        entry.idle_timer = Some(handle);
    }
}

/// The acquired connection handle; dropping it without [`Self::release`]
/// leaks the socket, mirroring the TypeScript release-closure contract.
pub(crate) struct AcquiredWebSocket {
    pub socket: Arc<dyn WebSocketLike>,
    /// The cache coordinates when this acquisition owns a cached entry.
    entry: Option<(String, String)>,
    pub reused: bool,
}

impl AcquiredWebSocket {
    /// Port of the `release({ keep })` closures.
    pub fn release(self, keep: bool) {
        let AcquiredWebSocket {
            socket,
            entry,
            reused: _,
        } = self;
        if !keep || !socket.is_reusable() {
            socket.close_silently(1000, "done");
            if let Some((session_id, account_id)) = entry {
                let mut state = ws_state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let remove = state
                    .session_cache
                    .get(&session_id)
                    .and_then(|accounts| accounts.get(&account_id))
                    .is_some_and(|cached| Arc::ptr_eq(&cached.socket, &socket));
                if remove {
                    remove_cache_entry(&mut state, &session_id, &account_id);
                }
            }
            return;
        }
        let Some((session_id, account_id)) = entry else {
            // A kept non-cached socket has nothing to keep it for.
            socket.close_silently(1000, "done");
            return;
        };
        let mut state = ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = state
            .session_cache
            .get_mut(&session_id)
            .and_then(|accounts| accounts.get_mut(&account_id))
        {
            if !Arc::ptr_eq(&cached.socket, &socket) {
                // A different connection replaced the entry.
                drop(state);
                socket.close_silently(1000, "done");
                return;
            }
            if let Some(timer) = cached.idle_timer.take() {
                timer.abort();
            }
            cached.busy = false;
        }
        drop(state);
        schedule_session_websocket_expiry(&session_id, &account_id);
    }

    /// Port of the error-path `entry.continuation = undefined`.
    pub fn clear_continuation(&self) {
        let Some((session_id, account_id)) = &self.entry else {
            return;
        };
        let mut state = ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = state
            .session_cache
            .get_mut(session_id)
            .and_then(|accounts| accounts.get_mut(account_id))
        {
            cached.continuation = None;
        }
    }

    /// Port of `buildCachedWebSocketRequestBody`'s cache read: the
    /// continuation snapshot used to build the delta request.
    pub(crate) fn cached_request_body(&self, body: &Value) -> Value {
        let Some((session_id, account_id)) = &self.entry else {
            return body.clone();
        };
        let mut state = ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cached) = state
            .session_cache
            .get_mut(session_id)
            .and_then(|accounts| accounts.get_mut(account_id))
        else {
            return body.clone();
        };
        build_cached_web_socket_request_body(cached, body)
    }

    /// Port of the continuation save at the end of a successful cached
    /// response.
    pub(crate) fn save_continuation(
        &self,
        last_request_body: Value,
        last_response_id: String,
        last_response_items: Value,
    ) {
        let Some((session_id, account_id)) = &self.entry else {
            return;
        };
        let mut state = ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = state
            .session_cache
            .get_mut(session_id)
            .and_then(|accounts| accounts.get_mut(account_id))
        {
            cached.continuation = Some(CachedWebSocketContinuation {
                last_request_body,
                last_response_id,
                last_response_items,
            });
        }
    }
}

/// Port of `connectWebSocket`: factory connect plus the open/error/close
/// race with the connect timeout and abort signal.
pub(crate) async fn connect_web_socket(
    url: &str,
    headers: Vec<(String, String)>,
    signal: Option<tokio_util::sync::CancellationToken>,
    connect_timeout_ms: Option<u64>,
) -> Result<Arc<dyn WebSocketLike>, CodexWsFailure> {
    register_cleanup_once();
    // Port of `headersToRecord(headers)` + the (lowercase-key no-op)
    // `delete wsHeaders["OpenAI-Beta"]`.
    let mut ws_headers: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        // `headersToRecord` lowercases every name; the TypeScript
        // `delete wsHeaders["OpenAI-Beta"]` targets the mixed-case key and
        // never matches, so the beta header stays in the record.
        ws_headers.push((name.to_lowercase(), value));
    }

    let socket = websocket_factory()(url.to_string(), ws_headers)
        .await
        .map_err(CodexWsFailure::transport)?;

    let timeout = connect_timeout_ms.filter(|timeout| *timeout > 0);
    loop {
        let wait_open = socket.next_event();
        tokio::select! {
            event = wait_open => match event {
                Some(WebSocketEvent::Open) => return Ok(socket),
                Some(WebSocketEvent::Error(message)) => {
                    return Err(CodexWsFailure::transport(message));
                }
                Some(WebSocketEvent::Close { code, reason }) => {
                    return Err(CodexWsFailure::transport(close_error_message(
                        code,
                        reason.as_deref(),
                    )));
                }
                // A data frame racing ahead of the open event has no
                // TypeScript analog (mocks always open first); keep waiting.
                Some(WebSocketEvent::Message(_)) => continue,
                None => return Err(CodexWsFailure::transport("WebSocket closed")),
            },
            () = async {
                match timeout {
                    Some(timeout) => tokio::time::sleep(Duration::from_millis(timeout)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                socket.close_silently(1000, "connect_timeout");
                let timeout = timeout.unwrap_or_default();
                return Err(CodexWsFailure::transport(format!(
                    "WebSocket connect timeout after {timeout}ms"
                )));
            },
            () = async {
                match &signal {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                socket.close_silently(1000, "aborted");
                return Err(CodexWsFailure::transport("Request was aborted"));
            },
        }
    }
}

/// Port of `acquireWebSocket`.
pub(crate) async fn acquire_web_socket(
    url: &str,
    headers: Vec<(String, String)>,
    session_id: Option<&str>,
    account_id: &str,
    signal: Option<tokio_util::sync::CancellationToken>,
    connect_timeout_ms: Option<u64>,
) -> Result<AcquiredWebSocket, CodexWsFailure> {
    let Some(session_id) = session_id.map(str::to_string) else {
        let socket = connect_web_socket(url, headers, signal, connect_timeout_ms).await?;
        return Ok(AcquiredWebSocket {
            socket,
            entry: None,
            reused: false,
        });
    };

    // Snapshot the cached entry's disposition under the lock; the
    // connect/reuse decisions below mirror the TypeScript branches.
    enum CachedDisposition {
        Reuse,
        ConnectUncached,
        ConnectCached,
    }
    let disposition = {
        let mut state = ws_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state
            .session_cache
            .get_mut(&session_id)
            .and_then(|accounts| accounts.get_mut(account_id))
        {
            Some(cached) => {
                if let Some(timer) = cached.idle_timer.take() {
                    timer.abort();
                }
                if !cached.busy && is_websocket_session_expired(cached) {
                    cached.socket.close_silently(1000, "connection_age_limit");
                    remove_cache_entry(&mut state, &session_id, account_id);
                    CachedDisposition::ConnectCached
                } else if !cached.busy && cached.socket.is_reusable() {
                    cached.busy = true;
                    CachedDisposition::Reuse
                } else if cached.busy {
                    CachedDisposition::ConnectUncached
                } else {
                    cached.socket.close_silently(1000, "done");
                    remove_cache_entry(&mut state, &session_id, account_id);
                    CachedDisposition::ConnectCached
                }
            }
            None => CachedDisposition::ConnectCached,
        }
    };

    match disposition {
        CachedDisposition::Reuse => {
            let socket = ws_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_cache
                .get(&session_id)
                .and_then(|accounts| accounts.get(account_id))
                .map(|cached| Arc::clone(&cached.socket))
                .expect("entry held busy under the lock");
            Ok(AcquiredWebSocket {
                socket,
                entry: Some((session_id, account_id.to_string())),
                reused: true,
            })
        }
        CachedDisposition::ConnectUncached => {
            let socket = connect_web_socket(url, headers, signal, connect_timeout_ms).await?;
            Ok(AcquiredWebSocket {
                socket,
                entry: None,
                reused: false,
            })
        }
        CachedDisposition::ConnectCached => {
            let socket = connect_web_socket(url, headers, signal, connect_timeout_ms).await?;
            let created_at = ws_now_ms();
            let mut state = ws_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .session_cache
                .entry(session_id.clone())
                .or_default()
                .insert(
                    account_id.to_string(),
                    CachedWebSocketConnection {
                        socket: Arc::clone(&socket),
                        busy: true,
                        created_at,
                        idle_timer: None,
                        continuation: None,
                    },
                );
            Ok(AcquiredWebSocket {
                socket,
                entry: Some((session_id, account_id.to_string())),
                reused: false,
            })
        }
    }
}

fn remove_cache_entry(state: &mut WsGlobalState, session_id: &str, account_id: &str) {
    if let Some(accounts) = state.session_cache.get_mut(session_id)
        && let Some(mut removed) = accounts.remove(account_id)
        && let Some(timer) = removed.idle_timer.take()
    {
        timer.abort();
    }
    if let Some(accounts) = state.session_cache.get(session_id)
        && accounts.is_empty()
    {
        state.session_cache.remove(session_id);
    }
}

// ---------------------------------------------------------------------------
// Continuation deltas
// ---------------------------------------------------------------------------

/// Port of `requestBodyWithoutInput` + `requestBodiesMatchExceptInput`: the
/// JSON serialization with `input` and `previous_response_id` removed.
fn request_body_without_input(body: &Value) -> Option<String> {
    let mut object = body.as_object()?.clone();
    object.remove("input");
    object.remove("previous_response_id");
    serde_json::to_string(&object).ok()
}

fn response_inputs_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    let empty = serde_json::Value::Array(Vec::new());
    serde_json::to_string(a.unwrap_or(&empty)).ok()
        == serde_json::to_string(b.unwrap_or(&empty)).ok()
}

/// Port of `getCachedWebSocketInputDelta`.
fn get_cached_web_socket_input_delta(
    body: &Value,
    continuation: &CachedWebSocketContinuation,
) -> Option<Value> {
    if request_body_without_input(body)?
        != request_body_without_input(&continuation.last_request_body)?
    {
        return None;
    }

    let current_input = body.get("input").and_then(Value::as_array)?;
    let last_input = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let last_items = continuation
        .last_response_items
        .as_array()
        .cloned()
        .unwrap_or_default();
    let baseline: Vec<Value> = last_input.into_iter().chain(last_items).collect();
    if current_input.len() < baseline.len() {
        return None;
    }

    let prefix: Vec<Value> = current_input.iter().take(baseline.len()).cloned().collect();
    let baseline_len = baseline.len();
    if !response_inputs_equal(Some(&Value::Array(prefix)), Some(&Value::Array(baseline))) {
        return None;
    }

    Some(Value::Array(
        current_input.iter().skip(baseline_len).cloned().collect(),
    ))
}

/// Port of `buildCachedWebSocketRequestBody` (mutates the entry when the
/// continuation is unusable, mirroring `entry.continuation = undefined`).
fn build_cached_web_socket_request_body(
    entry: &mut CachedWebSocketConnection,
    body: &Value,
) -> Value {
    let Some(continuation) = &entry.continuation else {
        return body.clone();
    };

    let delta = get_cached_web_socket_input_delta(body, continuation);
    match delta {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut request_body = body.clone();
            if let Some(object) = request_body.as_object_mut() {
                object.insert(
                    "previous_response_id".to_string(),
                    Value::String(continuation.last_response_id.clone()),
                );
                object.insert("input".to_string(), delta);
            }
            request_body
        }
        _ => {
            entry.continuation = None;
            body.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// The default factory (tokio-tungstenite)
// ---------------------------------------------------------------------------

enum SocketControl {
    Send(String),
    Close(u16, String),
}

/// The production [`WebSocketLike`]: a tungstenite socket with a pump task
/// feeding the shared event queue.
struct TungsteniteSocket {
    queue: WebSocketEventQueue,
    control: Mutex<Option<tokio::sync::mpsc::UnboundedSender<SocketControl>>>,
    open: std::sync::atomic::AtomicBool,
}

impl TungsteniteSocket {
    fn new<S>(socket: S) -> Self
    where
        S: futures::Stream<Item = Result<tungstenite::Message, tungstenite::Error>>
            + futures::Sink<tungstenite::Message, Error = tungstenite::Error>
            + Send
            + 'static,
    {
        use futures::{SinkExt, StreamExt};
        let queue = WebSocketEventQueue::default();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut sink, mut stream) = socket.split();
        let pump_queue = queue.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    control = control_rx.recv() => match control {
                        Some(SocketControl::Send(text)) => {
                            if sink
                                .send(tungstenite::Message::text(text))
                                .await
                                .is_err()
                            {
                                pump_queue.finish();
                                break;
                            }
                        }
                        Some(SocketControl::Close(code, reason)) => {
                            let frame = tungstenite::protocol::CloseFrame {
                                code: code.into(),
                                reason: reason.into(),
                            };
                            let _ = sink.send(tungstenite::Message::Close(Some(frame))).await;
                            pump_queue.finish();
                            break;
                        }
                        None => {
                            pump_queue.finish();
                            break;
                        }
                    },
                    message = stream.next() => match message {
                        Some(Ok(tungstenite::Message::Text(text))) => {
                            pump_queue.push(WebSocketEvent::Message(text.to_string()));
                        }
                        Some(Ok(tungstenite::Message::Binary(bytes))) => {
                            pump_queue.push(WebSocketEvent::Message(
                                String::from_utf8_lossy(&bytes).to_string(),
                            ));
                        }
                        Some(Ok(tungstenite::Message::Close(frame))) => {
                            pump_queue.push(WebSocketEvent::Close {
                                code: frame.as_ref().map(|frame| frame.code.into()),
                                reason: frame
                                    .map(|frame| frame.reason.to_string())
                                    .filter(|reason| !reason.is_empty()),
                            });
                            pump_queue.finish();
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            let message = error.to_string();
                            pump_queue.push(WebSocketEvent::Error(message));
                            pump_queue.finish();
                            break;
                        }
                        None => {
                            pump_queue.finish();
                            break;
                        }
                    },
                }
            }
        });
        let socket = Self {
            queue,
            control: Mutex::new(Some(control_tx)),
            open: std::sync::atomic::AtomicBool::new(true),
        };
        // The handshake already completed before the factory resolved, so
        // the adapter's connect wait sees the buffered open event.
        socket.queue.push(WebSocketEvent::Open);
        socket
    }
}

impl WebSocketLike for TungsteniteSocket {
    fn send_text(&self, data: &str) {
        let guard = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(control) = guard.as_ref() {
            let _ = control.send(SocketControl::Send(data.to_string()));
        }
    }

    fn close_silently(&self, code: u16, reason: &str) {
        let mut guard = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(control) = guard.take() {
            let _ = control.send(SocketControl::Close(code, reason.to_string()));
            self.open.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn is_reusable(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn next_event(&self) -> BoxFuture<'static, Option<WebSocketEvent>> {
        let queue = self.queue.clone();
        Box::pin(async move { queue.next().await })
    }
}

fn default_websocket_factory() -> WebSocketFactory {
    Arc::new(|url, headers| {
        Box::pin(async move {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|error| error.to_string())?;
            for (name, value) in headers {
                request.headers_mut().insert(
                    tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(
                        name.to_lowercase().as_bytes(),
                    )
                    .map_err(|error| error.to_string())?,
                    value
                        .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                        .map_err(|error| error.to_string())?,
                );
            }
            let (socket, _response) = tokio_tungstenite::connect_async(request)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Arc::new(TungsteniteSocket::new(socket)) as Arc<dyn WebSocketLike>)
        })
    })
}
