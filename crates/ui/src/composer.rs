//! The composer: a hand-rolled multiline text input ([`input`]), the
//! compact↔expanded flip ([`layout`], [`morph`]), the Send/Queue/Stop morph
//! ([`send_mode`]), optimistic send with failure recovery ([`send`]), per-chat
//! drafts and staged attachments ([`staging`]), the completion popups
//! ([`popups`]), file-mention chips ([`mentions`]), and the question wizard
//! ([`wizard`]) that replaces the composer while a run awaits input.
//!
//! Pure decision logic (flip, auto-grow math, button morph, wizard reducer,
//! pending-input detection) lives in free functions/structs with unit tests;
//! the gpui element only feeds them measurements.

mod input;
mod input_element;
mod layout;
mod mentions;
mod morph;
mod popups;
mod queue;
mod send;
mod send_mode;
mod slash;
mod staging;
mod wizard;

pub use input::*;
pub use layout::*;
pub use mentions::{SentMentionSpan, sent_mention_display};
pub use morph::*;
pub use send_mode::*;
pub use wizard::{Wizard, WizardStep};

use layout::composer_width_changed;
use popups::{FileMentionState, SlashState};

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use holt_proto::{ProviderId, SlashCommand};

use crate::image_viewer::{ImageViewer, ImageViewerEvent, ViewerTarget};
use crate::motion;
use crate::path_refs::PathRef;
use crate::pickers::Pickers;
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Composer wrapper
// ---------------------------------------------------------------------------

/// Events the shell listens for.
#[derive(Debug, Clone)]
pub enum ComposerEvent {
    /// A prompt was sent optimistically — give the transcript its exact row
    /// identity so it can anchor the prompt at the top with the reply's
    /// reserved space below it.
    Sent { chat_id: String, message_id: String },
}
pub struct Composer {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// Composer actions row: repo/branch/provider-model/traits (§1.7).
    /// Shared with the shell's new-session canvas, which renders the
    /// project target selector ([`Pickers::render_target_selectors`]).
    pickers: Entity<Pickers>,
    /// Draft text per chat key ("" = new-chat canvas), surviving navigation.
    drafts: HashMap<String, String>,
    /// Staged-but-unsent path references per chat key (picker/drag/paste/
    /// managed chips; path_refs.rs) — memory-only, key-swapped lifecycle,
    /// like `drafts`: navigating away and back restores them.
    path_refs: HashMap<String, Vec<PathRef>>,
    pasting: HashMap<String, usize>,
    preview_path_pending: Option<SharedString>,
    /// In-flight thumbnail loads (images.rs shared cache); keyed by path so
    /// concurrent renders never double-fetch.
    /// The image being viewed in the zoomable viewer (click a thumbnail).
    preview: Option<Entity<ImageViewer>>,
    /// The viewer's Closed subscription (owner clears the field + refocuses).
    viewer_close_sub: Option<gpui::Subscription>,
    /// Focus grab deferred to the next render (the `HOLT_ATTACH_PREVIEW`
    /// boot knob opens the viewer without a `Window`).
    preview_focus_pending: bool,
    /// In-flight file-picker prompt (paperclip).
    picker_task: Option<Task<()>>,
    mention_task: Option<Task<()>>,
    mention: FileMentionState,
    slash_task: Option<Task<()>>,
    /// In-flight `ListSkills` fetch for the slash popup (cwd-scoped —
    /// skills are provider-agnostic and never refetch per provider).
    slash_skills_task: Option<Task<()>>,
    slash: SlashState,
    /// Advertised commands per provider (one `ListCommands` per provider per
    /// composer lifetime; the engine caches discovery on its side too).
    slash_cache: HashMap<ProviderId, Vec<SlashCommand>>,
    /// Slash-popup row scroll — the stack overflows into a wheel/keyboard-
    /// scrollable list once it outgrows the card.
    slash_scroll: gpui::ScrollHandle,
    /// File-mention popup row scroll (same treatment).
    mention_scroll: gpui::ScrollHandle,
    /// Shared scrollbar hover/drag state for both popups' floating rails —
    /// they never show at once (mutually exclusive by token shape).
    popup_bar: crate::popover::MenuScrollbarState,
    current_key: String,
    sending: bool,
    failed_submissions: std::collections::HashMap<String, (String, serde_json::Value)>,
    failure: Option<SharedString>,
    /// The chat key `failure` belongs to (`None` = global, e.g. "Engine not
    /// connected"). Chat-scoped failures survive navigation and render only
    /// under their own chat — a blanket clear-on-switch erased the one
    /// visible trace of a failed send (2026-08-19).
    failure_key: Option<String>,
    wizard: Option<Wizard>,
    wizard_focus: FocusHandle,
    /// Inline editor for one pending queue message (queue.rs). Never touches
    /// `drafts` — the composer's own text is unrelated text being composed.
    queue_edit: Option<queue::QueueEdit>,
    /// Queue edit/delete RPC slot — a dropped in-flight save would strand
    /// `queue_busy` and freeze the row actions.
    queue_task: Option<Task<()>>,
    queue_busy: bool,
    queue_expanded: bool,
    /// In-flight height tween for the queue body's expand/collapse (queue.rs).
    /// `None` when settled — `begin_queue_disclosure_motion` stamps it on
    /// every toggle and the renderer reads `animating()` to decide whether
    /// to drive `with_animation` or jump to the target height.
    queue_motion: Option<queue::QueueDisclosureMotion>,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    answered_requests: HashSet<String>,
    advance_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    /// Interrupt/answer commands get their own slot: assigning `send_task`
    /// DROPPED an in-flight send future mid-upload — no banner, no cleanup,
    /// `sending` stuck true forever (2026-08-19 incident, "press Stop while
    /// a send grinds" shape).
    action_task: Option<Task<()>>,
    // -- compact/expanded flip state (hysteresis; see `composer_flip`) --
    /// Current layout mode (persisted across frames — never derived fresh).
    expanded_mode: bool,
    /// `layout_epoch` of the measurement that caused the last flip: the flip is
    /// re-evaluated only after the input has been laid out in the new mode, so
    /// at most one flip can happen per layout pass.
    flip_epoch: u64,
    /// Compact-mode input capacity, learned while compact (layout-stable).
    compact_capacity: f32,
    /// Input width first measured after expanding — container-width deltas
    /// while expanded shift `compact_capacity` by the same amount.
    expanded_anchor: f32,
    /// Last input width seen in the current mode (resize detection).
    last_seen_width: f32,
    /// Stable outer composer width supplied by the shell. Unlike Taffy's
    /// provisional input measurements, this changes only when the actual
    /// conversation column changes and can safely drive a follow-up render.
    last_available_width: Option<f32>,
    /// Set while an interactive resize is in flight; collapse is deferred
    /// until widths have settled for [`RESIZE_SETTLE_MS`].
    width_changed_at: Option<Instant>,
    settle_task: Option<Task<()>>,
    /// In-flight compact↔expanded morph (one per committed flip; manual
    /// drive — see [`FlipMorph`]).
    flip_morph: Option<FlipMorph>,
    /// Pill height actually rendered last frame — a committed flip morphs
    /// from here, so mid-flight reversals hand off without a jump.
    last_rendered_height: f32,
    /// Monotonic clock anchor for the morph timeline.
    morph_clock: Instant,
    /// Set on every session/route change: flips committed before this instant
    /// SNAP instead of morphing (see [`ROUTE_SNAP_MS`]).
    route_snap_until: Option<Instant>,
    _observe: Subscription,
    _pickers_observe: Subscription,
    _input_events: Subscription,
}
impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    /// The picker entity, for the shell's canvas target selectors.
    pub fn pickers(&self) -> &Entity<Pickers> {
        &self.pickers
    }

    /// Feed the stable conversation-column width into responsive composer
    /// controls.
    pub fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let composer_width = width.clamp(0.0, COMPOSER_MAX_WIDTH);
        if composer_width_changed(self.last_available_width, composer_width) {
            self.last_available_width = Some(composer_width);
            // The shell renders before this child, so this queues one more
            // pass after the input has been laid out at its final width. That
            // pass can consume the completed measurement without emitting an
            // event from inside Taffy's multi-pass measurement callback.
            cx.notify();
        }
    }

    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        crate::images::observe(cx);
        let input = cx.new(|cx| {
            let mut input = ComposerInput::new("Do anything…", cx);
            input.enable_mentions();
            input
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        // The footer toolbar (checkout kind + ref picker) is rendered INLINE
        // by the composer from picker state — a pickers-side notify (refs
        // loaded, popover toggled, pick made) must repaint the composer too.
        let pickers_observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.on_submit(cx),
            ComposerInputEvent::Edited | ComposerInputEvent::CursorMoved => {
                this.on_input_edited(cx)
            }
            ComposerInputEvent::ViewportChanged => cx.notify(),
            // The slash popup and the mention popup share the input's
            // completion key routing; they are mutually exclusive by token
            // shape (`/` at offset 0 vs `@` at a token boundary).
            ComposerInputEvent::MentionNavigate(delta) => {
                if this.slash.token.is_some() {
                    this.move_slash(*delta, cx)
                } else {
                    this.move_mention(*delta, cx)
                }
            }
            ComposerInputEvent::MentionAccept => {
                if this.slash.token.is_some() {
                    this.accept_slash(cx)
                } else {
                    this.accept_mention(cx)
                }
            }
            ComposerInputEvent::MentionDismiss => {
                if this.slash.token.is_some() {
                    this.dismiss_slash(cx)
                } else {
                    this.dismiss_mention(cx)
                }
            }
            ComposerInputEvent::PastedImages(images) => {
                this.stage_pasted_images(images.clone(), cx)
            }
            ComposerInputEvent::PastedPaths(paths) => this.add_paths(paths.clone(), cx),
            ComposerInputEvent::PreviewImage(path) => {
                this.preview_path_pending = Some(path.clone());
                cx.notify();
            }
        });
        let current_key = state.read(cx).selected_chat.clone().unwrap_or_default();
        let mut composer = Self {
            state,
            input,
            pickers,
            drafts: HashMap::new(),
            path_refs: HashMap::new(),
            pasting: HashMap::new(),
            preview_path_pending: None,
            preview: None,
            viewer_close_sub: None,
            preview_focus_pending: false,
            picker_task: None,
            mention_task: None,
            mention: FileMentionState::default(),
            slash_task: None,
            slash_skills_task: None,
            slash: SlashState::default(),
            slash_cache: HashMap::new(),
            slash_scroll: gpui::ScrollHandle::new(),
            mention_scroll: gpui::ScrollHandle::new(),
            popup_bar: crate::popover::MenuScrollbarState::default(),
            current_key,
            sending: false,
            failed_submissions: std::collections::HashMap::new(),
            failure: None,
            wizard: None,
            wizard_focus: cx.focus_handle(),
            queue_edit: None,
            queue_task: None,
            queue_busy: false,
            queue_expanded: true,
            queue_motion: None,
            answered_requests: HashSet::new(),
            failure_key: None,
            action_task: None,
            advance_task: None,
            send_task: None,
            expanded_mode: false,
            flip_epoch: 0,
            compact_capacity: 0.0,
            expanded_anchor: 0.0,
            last_seen_width: 0.0,
            last_available_width: None,
            width_changed_at: None,
            settle_task: None,
            flip_morph: None,
            last_rendered_height: 0.0,
            morph_clock: Instant::now(),
            route_snap_until: None,
            _observe: observe,
            _pickers_observe: pickers_observe,
            _input_events: input_events,
        };
        // Dev knob: pre-stage path references (drop/paste can't be
        // synthesized on a rig) — `HOLT_ATTACH=/path/a.png[,/path/b.png]`,
        // and `HOLT_ATTACH_PREVIEW=1` boots with the first image's viewer
        // open (focus lands on the first render).
        if let Ok(spec) = std::env::var("HOLT_ATTACH") {
            let refs: Vec<PathRef> = spec
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|path| {
                    match crate::path_refs::bind(std::path::Path::new(path.trim())) {
                        Ok(reference) => Some(reference),
                        Err(err) => {
                            tracing::warn!(%path, error = %err, "HOLT_ATTACH bind failed");
                            None
                        }
                    }
                })
                .collect();
            if std::env::var("HOLT_ATTACH_PREVIEW").is_ok_and(|v| v == "1") {
                composer.preview_focus_pending = true;
            }
            if !refs.is_empty() {
                composer
                    .path_refs
                    .entry(composer.current_key.clone())
                    .or_default()
                    .extend(refs);
            }
        }
        composer
    }

    /// Open the zoomable viewer over `targets[index]` (the draft's images).
    /// The composer subscribes to its Closed event to restore focus.
    pub(super) fn open_viewer(
        &mut self,
        targets: Vec<ViewerTarget>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let viewer = cx.new(|cx| ImageViewer::open(state, targets, index, cx));
        window.focus(&viewer.focus_handle(cx), cx);
        self.viewer_close_sub = Some(cx.subscribe_in(
            &viewer,
            window,
            |this, _, _: &ImageViewerEvent, window, cx| {
                this.preview = None;
                // Hand focus back to the input so typing (and the next
                // Escape) lands where it did before the viewer opened.
                let input_focus = this.input.read(cx).focus_handle.clone();
                window.focus(&input_focus, cx);
                cx.notify();
            },
        ));
        self.preview = Some(viewer);
        cx.notify();
    }

    /// Capture-knob passthrough (`HOLT_OPEN_DIALOG=model`): open the
    /// combined provider/model menu.
    pub fn debug_open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pickers
            .update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
    }

    pub fn is_sending(&self) -> bool {
        self.sending
    }
    fn render_input_with_completion(&self) -> gpui::Div {
        div().relative().child(self.input.clone())
    }
    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (key, pending) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone().unwrap_or_default(),
                pending_input_request(&s.transcript),
            )
        };

        // Draft swap on chat navigation — the input entity itself survives.
        if key != self.current_key {
            let old_text = self.input.read(cx).text().to_string();
            if old_text.is_empty() {
                self.drafts.remove(&self.current_key);
            } else {
                self.drafts.insert(self.current_key.clone(), old_text);
            }
            let draft = self.drafts.get(&key).cloned().unwrap_or_default();
            self.current_key = key;
            // `failure` deliberately survives navigation: chat-scoped
            // failures render only under their own chat (see `failure_key`),
            // so switching away and back must not erase the one visible
            // trace of a failed send.
            self.wizard = None;
            self.queue_edit = None;
            // Attachments stay stashed under their chat key (the map swap IS
            // the navigation); only the transient chrome resets.
            self.preview = None;
            self.reset_mention(None, cx);
            // Route changes snap (round 5/6): a mode difference between the
            // old and new session's composer must not glide across
            // navigation. Killing the in-flight morph here isn't enough —
            // the nav-driven flip only commits AFTER the swapped draft has
            // been re-measured, one or two renders later, so the whole
            // window snaps (see ROUTE_SNAP_MS).
            self.flip_morph = None;
            self.last_rendered_height = 0.0;
            self.route_snap_until = Some(Instant::now() + Duration::from_millis(ROUTE_SNAP_MS));
            self.input.update(cx, |input, cx| input.set_text(draft, cx));
        }

        // Question panel lifecycle (wizard state cached per request id).
        match pending {
            Some((request_id, questions)) if !self.answered_requests.contains(&request_id) => {
                let same = self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id);
                if !same {
                    self.reset_mention(None, cx);
                    self.wizard = Some(Wizard::new(request_id, questions));
                    self.advance_task = None;
                    // The shared input becomes the panel's free-text override.
                    self.input.update(cx, |input, cx| {
                        input.set_placeholder("Type your own answer, or pick an option above", cx)
                    });
                }
            }
            _ => {
                if let Some(wizard) = self.wizard.as_ref() {
                    // LATCH (original composer.tsx `inputLatch`): a transient
                    // fold/sync blip — or a steer appended behind the
                    // streaming entry — must not unmount the panel and lose
                    // the user's picks. Release only on explicit resolution
                    // (here or on another device) or when a NON-EMPTY
                    // transcript shows the question superseded (a newer
                    // assistant entry took over). Never on run death: the
                    // question stays answerable until answered — the engine
                    // delivers a dead run's answer as a resumed turn.
                    let transcript = self.state.read(cx).transcript.clone();
                    let released = input_request_resolved(&transcript, &wizard.request_id)
                        || (!transcript.is_empty()
                            && !self.answered_requests.contains(&wizard.request_id));
                    if released {
                        self.wizard = None;
                        self.advance_task = None;
                        self.input
                            .update(cx, |input, cx| input.set_placeholder("Do anything…", cx));
                    }
                }
            }
        }
        cx.notify();
    }
}

/// Focus lands on the prompt input (window-level focus fallbacks — e.g. after
/// the focused terminal panel is hidden — route here).
impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}
impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let wizard_active = self.wizard.is_some();
        if self.mention.token.is_some()
            && (wizard_active || !self.input.focus_handle(cx).is_focused(window))
        {
            self.reset_mention(None, cx);
        }
        if self.slash.token.is_some()
            && (wizard_active || !self.input.focus_handle(cx).is_focused(window))
        {
            self.reset_slash(None, cx);
        }
        let mode = self.button_mode(cx);
        let (text_width, has_newline, content_height, last_width, epoch) = {
            let input = self.input.read(cx);
            (
                input.measured_text_width(),
                input.has_newline(),
                input.measured_content_height(),
                input.last_width,
                input.layout_epoch,
            )
        };
        let now = Instant::now();
        // Only measurements taken *after* the last flip may drive the next one
        // (at most one flip per layout pass — a flip invalidates the widths).
        let measured_since_flip = epoch > self.flip_epoch && last_width > 0.0;
        if measured_since_flip {
            // A same-mode width change is an interactive window/pane resize:
            // defer collapse until sizes settle for RESIZE_SETTLE_MS. Expansion
            // remains live so compact controls never squeeze the input away.
            if self.last_seen_width > 0.0 && (last_width - self.last_seen_width).abs() > 0.5 {
                self.width_changed_at = Some(now);
            }
            self.last_seen_width = last_width;
            if self.expanded_mode {
                if self.expanded_anchor <= 0.0 {
                    self.expanded_anchor = last_width;
                }
            } else {
                // The compact pill's content box is the layout-stable capacity
                // both thresholds measure against.
                self.compact_capacity = last_width - 8.0;
            }
        }
        let resizing = self
            .width_changed_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(RESIZE_SETTLE_MS));
        if resizing && self.settle_task.is_none() {
            // Re-evaluate once the settle window has passed.
            self.settle_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_SETTLE_MS + 20))
                    .await;
                this.update(cx, |composer, cx| {
                    composer.settle_task = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        // Layout-stable compact capacity: measured directly while compact;
        // while expanded, the learned value shifted by any container resize
        // (the expanded input width tracks the container 1:1).
        let capacity = if !self.expanded_mode {
            if last_width > 0.0 {
                last_width - 8.0
            } else {
                f32::MAX // before first measure default to compact
            }
        } else if self.compact_capacity > 0.0 {
            if self.expanded_anchor > 0.0 && last_width > 0.0 {
                self.compact_capacity + (last_width - self.expanded_anchor)
            } else {
                self.compact_capacity
            }
        } else {
            f32::MAX
        };
        let next = composer_flip(
            self.expanded_mode,
            text_width,
            capacity,
            has_newline,
            resizing,
        );
        let committed_flip = next != self.expanded_mode && measured_since_flip;
        if committed_flip {
            self.expanded_mode = next;
            self.flip_epoch = epoch;
            self.expanded_anchor = 0.0;
            // The mode change moves the input width; don't read that jump as
            // an interactive resize.
            self.last_seen_width = 0.0;
        }
        // New chats render expanded regardless of `expanded_mode` (see below),
        // so a mode flip there changes nothing visible — never morph it.
        let new_chat = self.state.read(cx).selected_chat.is_none();
        // Morph clock in ms; dividing by the measurement knob stretches the
        // timeline exactly like shell.rs eval_tween's scaled duration.
        let now_ms = self.morph_clock.elapsed().as_secs_f32() * 1000.0 / motion::speed_scale();
        let route_snap = self
            .route_snap_until
            .is_some_and(|until| Instant::now() < until);
        self.flip_morph = flip_morph_step(
            self.flip_morph,
            committed_flip && !new_chat,
            self.last_rendered_height,
            now_ms,
            motion::reduced_motion(cx),
            route_snap,
        );
        let expanded = self.expanded_mode;

        // Chat-scoped failures render only under their own chat; a global
        // failure (no key) renders everywhere.
        let failure = self
            .failure
            .clone()
            .filter(|_| {
                self.failure_key
                    .as_ref()
                    .is_none_or(|key| *key == self.current_key)
            })
            .or_else(|| {
                (!self.sending && self.failed_submissions.contains_key(&self.current_key))
                    .then(|| "Message delivery was not confirmed.".into())
            });
        // Composer honesty: when the delivery path is degraded, say UP FRONT
        // that a send will queue (a durable local write delivered on
        // reconnect) instead of letting the button imply instant delivery.
        let queue_notice: Option<(SharedString, bool)> = {
            use holt_proto::ConnectivityState as S;
            let state = self.state.read(cx);
            let degraded = match state.selected_chat.as_deref() {
                Some(id) => state.chat_delivery_degraded(id),
                // New-chat canvas: judge by the connection itself.
                None => matches!(state.connectivity.state, S::Offline | S::Reconnecting),
            };
            let offline = state.connectivity.state == S::Offline;
            degraded.then(|| {
                let text: SharedString = if offline {
                    "Offline — messages will send when you're back online.".into()
                } else {
                    "Messages will send once the connection recovers.".into()
                };
                (text, offline)
            })
        };
        // Centered composer column (holt `mx-auto w-full max-w-3xl`).
        let container = div()
            .w_full()
            .max_w(px(COMPOSER_MAX_WIDTH))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            // Raw Escape (no popup/dialog consumed it) = interrupt the
            // running Turn while an Approval gates it (ADR-0014).
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_escape(event, cx);
            }))
            .when_some(failure, |el, message| {
                // holt composer.tsx `Notice` (matches the transcript
                // ErrorChip palette): `flex items-start gap-2 rounded-xl
                // border px-3 py-2 text-[12px] leading-snug` with a 14px
                // DangerTriangle — a subtle tinted wash, not a bare red
                // stroke. Amber for the offline-ish case (engine not
                // connected), red for send/run failures. Click dismisses.
                let offline = message.as_ref() == "Engine not connected";
                let (border_c, wash, text_c) = if offline {
                    let amber = theme.warning; // amber-400
                    let amber_200 = theme.warning_muted;
                    (
                        amber.opacity(0.16),
                        amber.opacity(0.05),
                        amber_200.opacity(0.9),
                    )
                } else {
                    let danger = theme.danger; // red-400
                    let red_300 = theme.danger_muted;
                    (
                        danger.opacity(0.16),
                        danger.opacity(0.05),
                        red_300.opacity(0.9),
                    )
                };
                el.child(
                    div()
                        .id("composer-failure")
                        .mx(px(4.0))
                        .mt(px(6.0))
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(border_c)
                        .bg(wash)
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(16.0))
                        .text_color(text_c)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.failed_submissions.contains_key(&this.current_key) {
                                return;
                            }
                            this.failure = None;
                            this.failure_key = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(14.0))
                                .mt(px(2.0))
                                .text_color(text_c),
                        )
                        .child(div().min_w_0().flex_1().child(message))
                        .when(
                            self.failed_submissions.contains_key(&self.current_key),
                            |el| {
                                el.child(
                                    div()
                                        .id("retry-message")
                                        .role(gpui::Role::Button)
                                        .aria_label("Retry message")
                                        .focusable()
                                        .px_2()
                                        .cursor_pointer()
                                        .hover(|el| el.bg(theme.glass_hover()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.retry_submission(cx);
                                        }))
                                        .on_key_down(cx.listener(
                                            |this, event: &gpui::KeyDownEvent, _, cx| {
                                                if matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                ) {
                                                    cx.stop_propagation();
                                                    this.retry_submission(cx);
                                                }
                                            },
                                        ))
                                        .child("Retry"),
                                )
                            },
                        ),
                )
            })
            .when_some(queue_notice, |el, (notice, offline)| {
                // Not a warning box (v0.2.12 feedback: the amber Notice read
                // as an error and flashed on every blip — pre-grace). One
                // quiet caption line, amber dot only for hard offline; it
                // clears itself the moment the path heals.
                let dot = if offline {
                    theme.warning
                } else {
                    theme.text_faint
                };
                el.child(crate::motion::fade_in(
                    "composer-queue-notice",
                    div()
                        .id("composer-queue-notice")
                        .mx(px(8.0))
                        .mt(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(11.0))
                        .line_height(px(14.0))
                        .text_color(theme.text_faint)
                        .child(div().size(px(5.0)).rounded_full().bg(dot))
                        .child(div().min_w_0().truncate().child(notice)),
                ))
            });

        if wizard_active {
            let wizard = self.render_wizard(cx);
            return container.child(motion::fade_quick("composer-wizard", div().child(wizard)));
        }

        // New chats always use the expanded layout: the repo/branch pickers
        // need the full-width actions row (holt composer-actions.tsx
        // `mustExpand = isNew || …`).
        let expanded = expanded || new_chat;

        // Committed-height morph: the layout below is already the NEW mode's;
        // only the pill's height (and the entrance fade/text glide driven by
        // `morph_t`) animates. Steady state renders exactly the target.
        // The staged strip (thumbnails + chips) adds its wrap height to the
        // pill in BOTH modes (it sits above the input row).
        let strip_width_hint = if last_width > 0.0 { last_width } else { 720.0 };
        let refs = self.staged_refs();
        let thumb_count = refs
            .iter()
            .filter(|reference| {
                !reference.is_dir && crate::images::is_image_path(&reference.path.to_string_lossy())
            })
            .count();
        let chip_count = refs.len() - thumb_count;
        let ref_strip_h = path_ref_strip_height(thumb_count, chip_count, strip_width_hint);
        let comment_strip_h = comment_strip_height(self.staged_comments(cx).len());
        let base_height = if expanded {
            composer_total_height(content_height)
        } else {
            COMPACT_TOTAL_HEIGHT
        };
        let target_height = base_height + ref_strip_h + comment_strip_h;
        let (pill_height, morph_t, morphing) = match self.flip_morph {
            Some(m) if !m.done(now_ms) => {
                (m.height(target_height, now_ms), m.progress(now_ms), true)
            }
            _ => (target_height, 1.0, false),
        };
        if !morphing {
            self.flip_morph = None;
        } else {
            // Manual tween drive: keep frames coming (shell.rs motion_active).
            window.request_animation_frame();
        }
        self.last_rendered_height = pill_height;

        let send_button = self.render_send_button(mode, cx);
        // Attach button — opens the native image picker (the original's hidden
        // `<input type=file accept="image/*" multiple>`); paste/drop also feed
        // the same strip. The parent action cluster owns the spacing: adding a
        // second margin here made the picker→attachment gap twice as wide as
        // attachment→send and made the paperclip look detached.
        let attach = div()
            .id("composer-attach")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            // holt composer-actions.tsx attach: `transition-colors`.
            .bg(motion::hover_blend(
                "composer-attach",
                gpui::transparent_black(),
                crate::theme::ink(0.10),
            ))
            .on_hover(motion::hover_listener("composer-attach"))
            .on_click(cx.listener(|this, _, _, cx| this.open_file_picker(cx)))
            .child(
                crate::icons::icon(crate::icons::PAPERCLIP)
                    .size(px(16.0))
                    // The source path's painted bounds are centered at x=11
                    // inside a 24px viewbox. Correct that optical offset while
                    // keeping the 28px hit target geometrically centered.
                    .relative()
                    .left(px(1.0))
                    .text_color(theme.text_muted),
            );
        // Staged strip (thumbnails + chips), above the input inside the
        // pill in both modes.
        let ref_strip = self.render_path_ref_strip(&theme, cx);
        let comments_chip = self.render_comments_chip(&theme, cx);

        // The pill chrome (holt composer.tsx): `rounded-[26px] border
        // border-white/[0.08] bg-white/[0.03] shadow-xl` — a floating pill with
        // a hairline over a faint wash, never a solid grey box. Picker chips,
        // attach, and the send circle all live INSIDE the pill.
        let pill_bg = theme.input_glass_bg();
        // No drop shadow on glass: it paints BEHIND the translucent fill and
        // shows through as an inner glow (theme.rs's card_selected_shadows
        // lesson; user report).
        let pill = div()
            .rounded(px(26.0))
            .bg(pill_bg)
            .border_1()
            .border_color(theme.border)
            .when(!theme.is_frost(), |el| el.shadow_lg());
        // The pill's bottom edge is stationary on screen (the composer sits at
        // the bottom of the shell column; growth moves the TOP edge), so the
        // controls pin to the bottom and only the text glides with the reveal
        // (round-9 follow-up: the send/attach/chips must not ride the height,
        // and none of them fade — the full cluster stays visible throughout).
        let cluster_dy = morph_cluster_dy(morph_t);
        let body = if expanded {
            // Expanded: textarea on top (`px-4 pb-1 pt-4`), actions row
            // (`px-3 pb-2.5 pt-1`, h-8 chips → 46px) ABSOLUTE at the pill's
            // stationary bottom — constant screen-y through the morph, with
            // the 2.5px compact↔expanded centering delta gliding out. The
            // text container is laid out at TARGET size (committed layout
            // never reflows mid-tween — the caret can't jump); its top pad
            // eases 12→16 so the first line glides from its compact resting
            // place. The whole control cluster stays at full alpha — chips,
            // attach and send are all (near-)stationary on the bottom anchor.
            let text_pt = morph_text_pad(morph_t);
            pill.h(px(pill_height))
                .overflow_hidden()
                .relative()
                .flex()
                .flex_col()
                .children(comments_chip)
                .children(ref_strip)
                .child(
                    div()
                        .h(px(
                            (base_height - PILL_BORDER_V - ACTIONS_ROW_HEIGHT).max(0.0)
                        ))
                        .px(px(16.0))
                        .pt(px(text_pt))
                        .pb(px(4.0))
                        .child(self.render_input_with_completion()),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(-cluster_dy))
                        .h(px(ACTIONS_ROW_HEIGHT))
                        .flex()
                        .flex_row()
                        .items_center()
                        // Shared group geometry (see CLUSTER_X_DELTA): the
                        // attachment belongs to the utility pickers, while
                        // Send has a larger structural separation.
                        .gap(px(ACTION_PRIMARY_GAP))
                        .pl(px(12.0))
                        .pr(px(morph_cluster_inset(true, morph_t)))
                        .pt(px(4.0))
                        .pb(px(10.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_end()
                                .gap(px(ACTION_UTILITY_GAP))
                                .child(self.pickers.clone())
                                .child(attach),
                        )
                        .child(send_button),
                )
        } else {
            // Compact pill: input and the actions cluster on one 47px line
            // (`py-3 pl-4 pr-2` textarea, `gap-2 py-1.5 pl-1 pr-2` cluster;
            // the 22.75px line centers to the same 12px inset as `py-3`).
            // The row is BOTTOM-justified: during the collapse morph the pill
            // top sweeps down over a stationary row, the text walks down from
            // its expanded resting place via a decaying relative offset, and
            // the whole inline cluster (chips + attach/send) holds its spot at
            // full alpha (2.5px centering delta gliding in).
            let text_glide = match self.flip_morph {
                Some(m) if morphing => collapse_text_glide(m.from, morph_t),
                _ => 0.0,
            };
            pill.h(px(pill_height))
                .overflow_hidden()
                .flex()
                .flex_col()
                .justify_end()
                .children(comments_chip)
                .children(ref_strip)
                .child(
                    div()
                        .h(px(COMPACT_TOTAL_HEIGHT - PILL_BORDER_V))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pl(px(16.0))
                                .pr(px(8.0))
                                .relative()
                                .top(px(-text_glide))
                                .child(self.render_input_with_completion()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                // Same utility/primary grouping as expanded;
                                // the right inset alone glides 12→8.
                                .gap(px(ACTION_PRIMARY_GAP))
                                .pl(px(4.0))
                                .pr(px(morph_cluster_inset(false, morph_t)))
                                .relative()
                                .top(px(-cluster_dy))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(ACTION_UTILITY_GAP))
                                        .child(self.pickers.clone())
                                        .child(attach),
                                )
                                .child(send_button),
                        ),
                )
        };
        // New sessions: the TARGET row (device + project chips) sits ABOVE
        // the pill, left-aligned like the checkout toolbar below it (user
        // request — moved off the canvas). Existing sessions name their
        // target in the titlebar instead.
        let container = if new_chat {
            let selectors = self
                .pickers
                .update(cx, |pickers, cx| pickers.render_target_selectors(cx));
            container.child(selectors)
        } else {
            container
        };
        let capability = self
            .pickers
            .read(cx)
            .selected_model(cx)
            .map(|m| m.image_capability)
            .unwrap_or_default();
        let has_images = !self.draft_image_targets(cx).is_empty();
        let container = container.when(has_images, |container| {
            let notice = match capability {
                holt_proto::ImageCapability::Unsupported => {
                    Some("This model cannot view images. Paths can still be sent.")
                }
                holt_proto::ImageCapability::Unknown => {
                    Some("Image support is unknown for this model.")
                }
                holt_proto::ImageCapability::Supported => None,
            };
            container.children(notice.map(|text| {
                div()
                    .px(px(16.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(text)
            }))
        });
        // The file dropzone lives in the shell (the whole conversation column,
        // not just the pill — shell.rs `chat-dropzone`); drops land back here
        // via `add_paths`.
        // Frosted: the pill backdrop-blurs the transcript scrolling under it
        // (the popover glass treatment; radius matches the pill's rounding).
        // Keep the queue in normal flow so the shell's bottom-stack measurement
        // reserves its actual height from the transcript.
        let container = container.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .px(px(20.0))
                        .child(self.render_message_queue(window, cx)),
                )
                .child(
                    div()
                        .relative()
                        // Tree-entry drops (ticket 09): a file or folder
                        // dragged from the File sidebar lands here as a path
                        // reference — the same staging the picker and OS-file
                        // drops use (bind to the live target, dedup, per-draft
                        // ownership). Internal drags never trigger the
                        // ExternalPaths veil above.
                        .rounded(px(26.0))
                        .drag_over::<crate::files::tree::TreeEntryDrag>(|style, _, _, _| {
                            style
                                .bg(crate::theme::wash(0.08))
                                .border_1()
                                .border_color(crate::theme::ink(0.18))
                        })
                        .on_drop::<crate::files::tree::TreeEntryDrag>(cx.listener(
                            |this, payload: &crate::files::tree::TreeEntryDrag, _, cx| {
                                this.add_paths(
                                    vec![std::path::PathBuf::from(payload.path.as_str())],
                                    cx,
                                );
                                cx.notify();
                            },
                        ))
                        .child(crate::frost::frosted(
                            26.0,
                            16.0,
                            motion::fade_quick("composer-input", body),
                        ))
                        // Both completion popups span the full pill width above it —
                        // the file-mention and slash tokens are mutually exclusive.
                        .children(self.render_file_mention_popup(&theme, cx))
                        .children(self.render_slash_popup(&theme, cx)),
                ),
        );
        // Branch/worktree toolbar under the pill (t3code BranchToolbar): the
        // checkout-kind selector + ref picker for new sessions, read-only
        // labels once the session exists. Git spaces only.
        let footer = self
            .pickers
            .update(cx, |pickers, cx| pickers.render_footer(cx));
        let container = match footer {
            Some(footer) => container.child(footer),
            None => container,
        };
        crate::images::flush_evicted(Some(window), cx);
        if let Some(path) = self.preview_path_pending.take() {
            let targets = self.draft_image_targets(cx);
            if let Some(index) = targets.iter().position(|t| t.path == path) {
                self.open_viewer(targets, index, window, cx);
            }
        }
        if std::mem::take(&mut self.preview_focus_pending) && self.preview.is_none() {
            let targets: Vec<_> = self
                .staged_refs()
                .iter()
                .filter(|r| !r.is_dir && crate::images::is_image_path(&r.full_path()))
                .map(|r| ViewerTarget {
                    path: r.full_path().into(),
                    label: r.name().into(),
                })
                .collect();
            if !targets.is_empty() {
                self.open_viewer(targets, 0, window, cx);
            }
        }
        // The zoomable image viewer (a shared modal entity) — when open it
        // is the composer's whole tail.
        if let Some(viewer) = self.preview.clone() {
            if std::mem::take(&mut self.preview_focus_pending) {
                window.focus(&viewer.focus_handle(cx), cx);
            }
            return container.child(viewer);
        }
        container
    }
}
