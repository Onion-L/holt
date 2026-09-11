//! OS banner notifications for background Turn completion (ADR-0019). One
//! application-scoped [`TurnNotificationController`] consumes the engine's
//! `WatchTurnTerminalEvents` stream for the life of the process — it is
//! created during UI bootstrap and is never tied to a window or Shell
//! instance, so closing and reopening the main window neither duplicates the
//! listener nor the response handler.
//!
//! Interaction: banners are tagged by Chat id (same-Chat replacement,
//! cross-Chat retention). A body click retracts the banner, activates Holt
//! (rebuilding the main window when none is open), and selects the Chat when
//! it still exists; a stale tag only activates Holt and retracts. Opening
//! and marking a Chat seen through the normal UI also retracts its banner
//! (see [`crate::state::AppState::mark_chat_seen`]).
//!
//! Delivery is best-effort and silent: denied OS permission, missing
//! bundle/package support, unavailable notification services, and retraction
//! limits produce no in-app toast, modal, Transcript notice, or fallback
//! audio (the platform layer no-ops).

use std::collections::{HashSet, VecDeque};

use gpui::{
    App, AppContext as _, Context, Entity, Subscription, SystemNotification,
    SystemNotificationResponse, Task,
};
use holt_rpc::methods;
use holt_rpc::turns::{TurnOutcome, TurnTerminalEvent};

use crate::settings;
use crate::state::AppState;
use crate::watch_coordinator::WatchCoordinator;

/// Process-wide application identity, registered once during early startup
/// (before windows open or notifications post) — platforms that cannot post
/// notifications for an unidentified process stay silent without it.
pub const APP_IDENTITY: &str = "dev.holt.app";
/// User-visible application name on notifications and the OS app surfaces.
pub const APP_DISPLAY_NAME: &str = "Holt";

/// Bound on the dedup memory of already-handled `eventId`s. The engine emits
/// one event per settled Turn; duplicates only come from transport re-
/// delivery or resubscription, so a small recent window is ample.
const DEDUP_CAPACITY: usize = 256;

/// Application-scoped consumer of Turn terminal events. Owns the
/// `WatchTurnTerminalEvents` subscription and the single process-wide
/// notification response handler.
pub struct TurnNotificationController {
    state: Entity<AppState>,
    pump: Option<Task<()>>,
    seen_event_ids: VecDeque<String>,
    seen_event_id_set: HashSet<String>,
    _state_observation: Subscription,
}

impl TurnNotificationController {
    /// Create the controller during UI bootstrap. The returned entity must be
    /// held by an application-scoped owner (a global) so the subscription and
    /// response handler outlive every window.
    pub fn init(state: Entity<AppState>, cx: &mut App) -> Entity<Self> {
        let controller = cx.new(|cx| {
            let observation = cx.observe(&state, |this: &mut Self, _, cx| this.ensure_pump(cx));
            Self {
                state,
                pump: None,
                seen_event_ids: VecDeque::new(),
                seen_event_id_set: HashSet::new(),
                _state_observation: observation,
            }
        });
        // The one process-wide response handler, owned here rather than by
        // any window: closing and reopening the main window never touches
        // this registration.
        let weak = controller.downgrade();
        cx.on_system_notification_response(move |response, cx| {
            weak.update(cx, |this, cx| this.handle_response(response, cx))
                .ok();
        });
        controller.update(cx, |this, cx| this.ensure_pump(cx));
        controller
    }

    /// Start the standing subscription once the engine is attached. The
    /// engine arrives asynchronously after bootstrap (and can be re-attached
    /// after a failed boot), so this is retried off the AppState observation
    /// until a client exists.
    fn ensure_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.pump = Some(cx.spawn(async move |this, cx| {
            // Resubscribe loop, same contract as the standing AppState
            // watches: a dropped stream retries instead of going silent. The
            // watch is live-only — reconnecting never replays events, so a
            // resubscribe cannot repost a completion.
            loop {
                let mut rx = match engine
                    .client()
                    .subscribe(methods::WATCH_TURN_TERMINAL_EVENTS, serde_json::json!({}))
                    .await
                {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, "turn terminal events watch unavailable; retrying");
                        if this.update(cx, |_, _| {}).is_err() {
                            return;
                        }
                        cx.background_executor()
                            .timer(WatchCoordinator::RETRY_DELAY)
                            .await;
                        continue;
                    }
                };
                while let Some(value) = rx.recv().await {
                    let event: TurnTerminalEvent = match WatchCoordinator::decode(value) {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!(error = %err, "dropping malformed turn terminal event");
                            continue;
                        }
                    };
                    if this
                        .update(cx, |controller, cx| controller.handle_event(event, cx))
                        .is_err()
                    {
                        return;
                    }
                }
                tracing::debug!("turn terminal events stream ended; resubscribing");
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(WatchCoordinator::RETRY_DELAY)
                    .await;
            }
        }));
    }

    /// One terminal event. Settings and application activation are read HERE,
    /// at handling time, so a choice made mid-Turn applies when the Turn
    /// finishes.
    fn handle_event(&mut self, event: TurnTerminalEvent, cx: &mut Context<Self>) {
        if !self.remember_event(&event.event_id) {
            return;
        }
        let succeeded = match event.outcome {
            // The user's own cancellation never alerts.
            TurnOutcome::Interrupted => return,
            TurnOutcome::Succeeded => true,
            TurnOutcome::Failed => false,
        };
        let settings = settings::current(cx);
        if !settings.completion_notifications {
            return;
        }
        // Any active Holt window suppresses the banner. A running app with no
        // open window has no active window and stays eligible.
        if cx.active_window().is_some() {
            return;
        }
        let chat_title = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == event.chat_id)
            .and_then(|chat| chat.title.as_deref())
            .map(str::trim)
            .filter(|title| !title.is_empty());
        // Privacy: the body is the Chat title plus the bare outcome — never
        // answer text, reasoning, tool output, paths, or the engine's
        // internal failure reason.
        let body = match (chat_title, succeeded) {
            (Some(title), true) => format!("{title} completed"),
            (Some(title), false) => format!("{title} failed"),
            (None, true) => "Chat completed".to_string(),
            (None, false) => "Chat failed".to_string(),
        };
        // Tagged by Chat identity: a newer result replaces the older banner
        // for that Chat while other Chats stay distinct. Every eligible event
        // posts independently so the OS plays its sound each time.
        cx.show_system_notification(SystemNotification {
            tag: event.chat_id.into(),
            title: APP_DISPLAY_NAME.into(),
            body: body.into(),
            actions: Vec::new(),
            sound: settings.completion_notification_sound,
        });
    }

    /// Dedup by `eventId` against a bounded recent window. Returns false for
    /// an already-handled event. An event without a usable id cannot be
    /// deduplicated and is always handled.
    fn remember_event(&mut self, event_id: &str) -> bool {
        if event_id.is_empty() {
            return true;
        }
        if !self.seen_event_id_set.insert(event_id.to_string()) {
            return false;
        }
        self.seen_event_ids.push_back(event_id.to_string());
        while self.seen_event_ids.len() > DEDUP_CAPACITY {
            if let Some(oldest) = self.seen_event_ids.pop_front() {
                self.seen_event_id_set.remove(&oldest);
            }
        }
        true
    }

    /// Process-wide notification response entry point (registered once in
    /// [`Self::init`]). Holt posts no action buttons, so every response is a
    /// body click. The tag is the Chat id the banner was posted for.
    fn handle_response(&mut self, response: SystemNotificationResponse, cx: &mut Context<Self>) {
        let chat_id = response.tag.to_string();
        // The click always retracts its own banner first — even a stale tag
        // must leave Notification Center. Best-effort: platforms that cannot
        // retract a delivered notification no-op and let it age out.
        cx.dismiss_system_notification(&chat_id);
        // Bring Holt forward at the OS level; raising an actual window
        // happens below (a live process can have no window at all).
        cx.activate(true);
        let chat_exists = self
            .state
            .read(cx)
            .chats
            .iter()
            .any(|chat| chat.id == chat_id);
        if !chat_exists {
            // Deleted or otherwise unresolvable Chat: activate + retract
            // only. No data recreation, no error dialog.
            return;
        }
        // ⌘W on macOS keeps the process alive with no window: rebuild the
        // normal main window before selecting into it.
        if cx.windows().is_empty()
            && let Some(open) = cx
                .try_global::<MainWindowProvider>()
                .map(|provider| provider.open)
        {
            open(cx);
        }
        if let Some(window) = cx.windows().first().copied() {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        }
        // Normal selection semantics: this lands in the Chat's space and
        // marks it seen (which re-requests dismissal — idempotent).
        self.state
            .update(cx, |state, cx| state.select_chat(Some(chat_id), cx));
    }
}

/// Application-scoped owner keeping the controller (and with it the event
/// subscription and response handler) alive for the process lifetime.
struct TurnNotificationsGlobal {
    _controller: Entity<TurnNotificationController>,
}

impl gpui::Global for TurnNotificationsGlobal {}

/// How the controller rebuilds the normal main window when a notification
/// click arrives with no window open. A plain fn pointer (not a closure) so
/// it can be copied out of the global before `&mut App` is handed back;
/// `run_app` installs the real window opener, tests install their own.
struct MainWindowProvider {
    open: fn(&mut App),
}

impl gpui::Global for MainWindowProvider {}

/// Register the main-window opener used by notification navigation. Called
/// once from `run_app` during bootstrap.
pub fn set_main_window_provider(open: fn(&mut App), cx: &mut App) {
    cx.set_global(MainWindowProvider { open });
}

/// Create the controller and anchor it to the process. Called once from
/// `run_app` during UI bootstrap, before the main window opens.
pub fn install(state: Entity<AppState>, cx: &mut App) {
    let controller = TurnNotificationController::init(state, cx);
    cx.set_global(TurnNotificationsGlobal {
        _controller: controller,
    });
}
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::StreamExt as _;
    use gpui::{
        Bounds, IntoElement, Render, TestAppContext, Window, WindowBounds, WindowOptions, div,
    };
    use holt_rpc::turns::TurnOutcome;
    use holt_rpc::{RpcError, RpcReply, RpcService};

    use super::*;
    use crate::settings::{SavePolicy, UiSettings};
    use crate::theme::Theme;

    /// Controlled terminal-event stream: each subscription gets its own
    /// sender; the test pushes events in. Chats frames are served from the
    /// same fake so titles arrive through WatchChats like production.
    struct FakeEngine {
        events: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
        chats: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
    }

    #[derive(Clone)]
    struct FakeEngineHandle {
        engine: Arc<FakeEngine>,
    }

    impl FakeEngineHandle {
        fn client(&self, runtime: &tokio::runtime::Runtime) -> holt_rpc::RpcClient {
            // `memory_client` spawns its dispatch loop with `tokio::spawn`,
            // which needs a runtime context on this thread.
            let _guard = runtime.enter();
            holt_rpc::memory_client(self.engine.clone())
        }

        fn push_event(&self, event: TurnTerminalEvent) {
            let value = serde_json::to_value(event).unwrap();
            self.engine
                .events
                .lock()
                .unwrap()
                .retain(|tx| tx.send(value.clone()).is_ok());
        }

        fn push_chats(&self, chats: serde_json::Value) {
            self.engine
                .chats
                .lock()
                .unwrap()
                .retain(|tx| tx.send(chats.clone()).is_ok());
        }
    }

    #[async_trait::async_trait]
    impl RpcService for FakeEngine {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            let source = match method {
                methods::WATCH_TURN_TERMINAL_EVENTS => &self.events,
                methods::WATCH_CHATS => &self.chats,
                // The standing AppState watches (sessions, spaces,
                // connectivity, auth) are irrelevant here — UnknownMethod
                // parks their pumps on the retry timer.
                _ => return Err(RpcError::UnknownMethod(method.to_string())),
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            source.lock().unwrap().push(tx);
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|value| (value, rx))
            });
            Ok(RpcReply::Stream(stream.boxed()))
        }
    }

    fn event(event_id: &str, chat_id: &str, outcome: TurnOutcome) -> TurnTerminalEvent {
        TurnTerminalEvent {
            event_id: event_id.into(),
            chat_id: chat_id.into(),
            message_id: format!("message-{event_id}"),
            outcome,
            finished_at: 1_760_000_000_000,
            internal_reason: None,
            change_set: None,
        }
    }

    fn chat_json(id: &str, title: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "deviceId": "test-device",
            "title": title,
            "archived": false,
            "createdAt": "2026-09-07T00:00:00Z",
        })
    }

    struct Harness {
        state: Entity<AppState>,
        _controller: Entity<TurnNotificationController>,
        _data_dir: tempfile::TempDir,
        // Current-thread runtime so the RPC dispatch loop runs on the TEST
        // thread when pumped (gpui's test scheduler forbids cross-thread
        // activity); `block_on` below is the pump.
        runtime: tokio::runtime::Runtime,
        engine: FakeEngineHandle,
    }

    impl Harness {
        /// Drive RPC dispatch a few rounds (test thread), then the gpui
        /// foreground executor.
        fn pump(&self, cx: &TestAppContext) {
            for _ in 0..4 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                cx.run_until_parked();
            }
        }

        /// Pump until `condition` holds (bounded; the assertion after it
        /// reports the failure).
        fn wait_until(&self, cx: &TestAppContext, condition: impl Fn(&TestAppContext) -> bool) {
            for _ in 0..100 {
                self.pump(cx);
                if condition(cx) {
                    return;
                }
            }
            self.pump(cx);
        }

        /// Bounded settle for paths where nothing should ever arrive.
        fn flush(&self, cx: &TestAppContext) {
            for _ in 0..10 {
                self.pump(cx);
            }
        }
    }

    /// Boot the controller against the fake engine: settings store seeded by
    /// `configure`, identity set (the test platform refuses notifications
    /// for an unidentified process, like the real ones), and the main-window
    /// provider wired the way `run_app` wires it.
    fn harness(cx: &mut TestAppContext, configure: impl FnOnce(&mut UiSettings)) -> Harness {
        let engine = FakeEngineHandle {
            engine: Arc::new(FakeEngine {
                events: Mutex::new(Vec::new()),
                chats: Mutex::new(Vec::new()),
            }),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            let mut settings = UiSettings::default();
            configure(&mut settings);
            settings::init(settings, data_dir.path(), cx);
            cx.set_app_identity(APP_IDENTITY, APP_DISPLAY_NAME);
            set_main_window_provider(open_blank_main_window, cx);
        });
        let state = cx.new(|_| AppState::new());
        let controller = cx.update(|cx| TurnNotificationController::init(state.clone(), cx));
        let client = engine.client(&runtime);
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let harness = Harness {
            state,
            _controller: controller,
            _data_dir: data_dir,
            runtime,
            engine,
        };
        harness.pump(cx);
        harness
    }

    fn shown(cx: &TestAppContext) -> Vec<SystemNotification> {
        cx.shown_system_notifications()
    }

    fn delivered(cx: &TestAppContext) -> Vec<SystemNotification> {
        cx.delivered_system_notifications()
    }

    fn dismissed(cx: &TestAppContext) -> Vec<String> {
        cx.dismissed_system_notifications()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn chats_len(harness: &Harness, cx: &TestAppContext) -> usize {
        cx.read(|cx| harness.state.read(cx).chats.len())
    }

    fn selected_chat(harness: &Harness, cx: &TestAppContext) -> Option<String> {
        cx.read(|cx| harness.state.read(cx).selected_chat.clone())
    }

    struct BlankView;

    impl Render for BlankView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// Stand-in for the main-window opener `run_app` registers: builds a
    /// blank window where production rebuilds the Shell.
    fn open_blank_main_window(cx: &mut App) {
        if !cx.windows().is_empty() {
            return;
        }
        let bounds = Bounds::maximized(None, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| BlankView),
        )
        .unwrap();
    }

    fn respond(cx: &TestAppContext, tag: &str) {
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: tag.into(),
            action_id: None,
        });
        cx.run_until_parked();
    }

    /// Open a window and drive the platform's active-window state explicitly.
    fn active_window(cx: &mut TestAppContext) -> gpui::VisualTestContext {
        let window = cx.add_window(|_window, _cx| BlankView);
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        gpui::VisualTestContext::from_window(*window, cx)
    }

    #[gpui::test]
    async fn succeeded_event_posts_titled_banner(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([chat_json(
            "chat-1",
            Some("Fix the flake")
        )]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());

        let shown = shown(cx);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title, "Holt");
        assert_eq!(shown[0].body, "Fix the flake completed");
        assert_eq!(shown[0].tag, "chat-1");
        assert!(shown[0].sound);
    }

    #[gpui::test]
    async fn failed_event_posts_failed_wording_without_reason(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([chat_json(
            "chat-1",
            Some("Refactor auth")
        )]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);
        let mut failed = event("event-1", "chat-1", TurnOutcome::Failed);
        failed.internal_reason = Some("provider 500 at /secret/path with answer text".into());
        harness.engine.push_event(failed);
        harness.wait_until(cx, |cx| !shown(cx).is_empty());

        let shown = shown(cx);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].body, "Refactor auth failed");
        let payload = format!("{} {}", shown[0].title, shown[0].body);
        for forbidden in ["provider 500", "/secret/path", "answer text"] {
            assert!(
                !payload.contains(forbidden),
                "notification must not leak {forbidden:?}: {payload}"
            );
        }
    }

    #[gpui::test]
    async fn interrupted_event_never_posts(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Interrupted));
        harness.flush(cx);
        assert!(shown(cx).is_empty());
    }

    #[gpui::test]
    async fn empty_or_missing_title_uses_neutral_wording(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([
            chat_json("chat-empty", Some("   ")),
            chat_json("chat-missing", None),
        ]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 2);
        harness
            .engine
            .push_event(event("event-1", "chat-empty", TurnOutcome::Succeeded));
        harness
            .engine
            .push_event(event("event-2", "chat-missing", TurnOutcome::Failed));
        // A chat unknown to the registry also falls back.
        harness
            .engine
            .push_event(event("event-3", "chat-unknown", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| shown(cx).len() == 3);

        let shown = shown(cx);
        assert_eq!(shown.len(), 3);
        assert_eq!(shown[0].body, "Chat completed");
        assert_eq!(shown[1].body, "Chat failed");
        assert_eq!(shown[2].body, "Chat completed");
    }

    #[gpui::test]
    async fn an_active_window_suppresses_any_chat(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        let _visual = active_window(cx);
        // Even the selected chat stays quiet while any Holt window is active.
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.flush(cx);
        assert!(shown(cx).is_empty());
    }

    #[gpui::test]
    async fn backgrounded_window_delivers_again(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        let mut visual = active_window(cx);
        visual.deactivate_window();
        cx.run_until_parked();
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        assert_eq!(shown(cx).len(), 1);
    }

    #[gpui::test]
    async fn master_toggle_off_suppresses_banner_and_sound(cx: &mut TestAppContext) {
        let harness = harness(cx, |settings| {
            settings.completion_notifications = false;
            settings.completion_notification_sound = false;
        });
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.flush(cx);
        assert!(shown(cx).is_empty());
    }

    #[gpui::test]
    async fn settings_changed_mid_turn_are_read_at_event_time(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        // The Turn is "running" while the user disables notifications.
        cx.update(|cx| {
            settings::update(SavePolicy::Immediate, cx, |settings| {
                settings.completion_notifications = false;
            });
        });
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.flush(cx);
        assert!(shown(cx).is_empty());
        // Re-enabled: the next Turn's event notifies again.
        cx.update(|cx| {
            settings::update(SavePolicy::Immediate, cx, |settings| {
                settings.completion_notifications = true;
            });
        });
        harness
            .engine
            .push_event(event("event-2", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        assert_eq!(shown(cx).len(), 1);
    }

    #[gpui::test]
    async fn sound_toggle_reaches_the_platform_payload(cx: &mut TestAppContext) {
        let harness = harness(cx, |settings| {
            settings.completion_notification_sound = false;
        });
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        let shown = shown(cx);
        assert_eq!(shown.len(), 1);
        assert!(!shown[0].sound);
    }

    #[gpui::test]
    async fn same_chat_replaces_older_banner_others_stay(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        for (event_id, chat_id) in [
            ("event-1", "chat-1"),
            ("event-2", "chat-2"),
            ("event-3", "chat-1"),
        ] {
            harness
                .engine
                .push_event(event(event_id, chat_id, TurnOutcome::Succeeded));
        }
        harness.wait_until(cx, |cx| shown(cx).len() == 3);

        // Every event posts independently (sound per completion)…
        assert_eq!(shown(cx).len(), 3);
        // …but per-Chat tags replace, keeping distinct Chats separate.
        let delivered = cx.delivered_system_notifications();
        assert_eq!(delivered.len(), 2);
        assert!(
            delivered
                .iter()
                .any(|notification| notification.tag == "chat-1")
        );
        assert!(
            delivered
                .iter()
                .any(|notification| notification.tag == "chat-2")
        );
    }

    #[gpui::test]
    async fn duplicate_event_id_posts_once(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        harness.flush(cx);
        assert_eq!(shown(cx).len(), 1);
    }

    #[gpui::test]
    async fn response_selects_existing_chat_and_retracts_banner(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([chat_json(
            "chat-1",
            Some("Fix the flake")
        )]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);
        // A window is open but in the background, so the banner posts.
        let _window = cx.add_window(|_window, _cx| BlankView);
        cx.run_until_parked();
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        assert_eq!(delivered(cx).len(), 1);

        respond(cx, "chat-1");

        assert_eq!(selected_chat(&harness, cx).as_deref(), Some("chat-1"));
        assert!(delivered(cx).is_empty());
        assert!(dismissed(cx).contains(&"chat-1".to_string()));
        // The click raised the open window.
        assert!(cx.read(|cx| cx.active_window().is_some()));
    }

    #[gpui::test]
    async fn response_with_no_window_reopens_main_window_then_selects(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([chat_json(
            "chat-1",
            Some("Fix the flake")
        )]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        assert!(cx.read(|cx| cx.windows().is_empty()));

        respond(cx, "chat-1");

        assert_eq!(cx.read(|cx| cx.windows().len()), 1);
        assert_eq!(selected_chat(&harness, cx).as_deref(), Some("chat-1"));
        assert!(delivered(cx).is_empty());
        assert!(cx.read(|cx| cx.active_window().is_some()));
    }

    #[gpui::test]
    async fn response_for_deleted_chat_only_activates_and_retracts(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        // No Chat rows: the tag names a deleted conversation. The banner was
        // posted before the deletion reached this device.
        harness
            .engine
            .push_event(event("event-1", "chat-gone", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());

        respond(cx, "chat-gone");

        assert!(delivered(cx).is_empty());
        assert_eq!(dismissed(cx), vec!["chat-gone".to_string()]);
        // No data recreation, no window, no selection.
        assert_eq!(chats_len(&harness, cx), 0);
        assert_eq!(selected_chat(&harness, cx), None);
        assert!(cx.read(|cx| cx.windows().is_empty()));
    }

    #[gpui::test]
    async fn one_response_handler_survives_window_close_and_reopen(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        harness.engine.push_chats(serde_json::json!([chat_json(
            "chat-1",
            Some("Fix the flake")
        )]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);

        // Close and reopen the main window; the handler must not duplicate.
        let window = cx.add_window(|_window, _cx| BlankView);
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.windows().is_empty()));
        let _reopened = cx.add_window(|_window, _cx| BlankView);
        cx.run_until_parked();

        // A stale tag counts handler invocations exactly: the stale path
        // dismisses once per invocation and touches nothing else.
        respond(cx, "chat-gone");
        assert_eq!(dismissed(cx), vec!["chat-gone".to_string()]);

        // …and the same single handler still navigates.
        respond(cx, "chat-1");
        assert_eq!(selected_chat(&harness, cx).as_deref(), Some("chat-1"));
    }

    #[gpui::test]
    async fn opening_and_marking_seen_retracts_outstanding_banner(cx: &mut TestAppContext) {
        let harness = harness(cx, |_| {});
        // A Turn just finished, so the Chat is unseen (lastMessageAt newer
        // than any seen marker).
        let mut chat = chat_json("chat-1", Some("Fix the flake"));
        chat["lastMessageAt"] = serde_json::json!("2026-09-07T01:00:00Z");
        harness.engine.push_chats(serde_json::json!([chat]));
        harness.wait_until(cx, |cx| chats_len(&harness, cx) == 1);
        harness
            .engine
            .push_event(event("event-1", "chat-1", TurnOutcome::Succeeded));
        harness.wait_until(cx, |cx| !shown(cx).is_empty());
        assert_eq!(delivered(cx).len(), 1);

        // The user opens the Chat through the normal UI instead of clicking
        // the banner.
        harness.state.update(cx, |state, cx| {
            state.select_chat(Some("chat-1".into()), cx);
        });
        harness.flush(cx);

        assert!(delivered(cx).is_empty());
        assert!(dismissed(cx).contains(&"chat-1".to_string()));
        // Seen semantics are unchanged: the open still marked the Chat seen.
        let unseen = cx.read(|cx| harness.state.read(cx).chats[0].unseen());
        assert!(!unseen);
    }
}
