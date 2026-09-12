//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups
//!   (agent/spawn chips split out so they never collapse) and input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! The list anchors at the TOP (ADR-0008): rows lay out document-style and a
//! short transcript leaves empty space below rather than rising from the
//! pane's bottom. Tail-following is still ours, not the list's.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band. A send holds its prompt in place when the reply has
//! room below it (a frozen per-anchor inset) or glides it to the viewport top
//! otherwise, and hands off to the same glide when the reply overflows.
//! While that anchor holds, wheel/touch is clamped rather than obeyed — the
//! whole turn is already visible, so there is nothing to scroll to.
//!
//! Module layout: this facade owns the `Transcript` entity — state, sync,
//! scroll/own-turn stepping, and the shell-facing API/events; the children
//! own pure and rendering concerns: [`model`] (rows and fingerprints),
//! [`markdown`] (parse wiring and thought flattening), [`viewport`] (spring,
//! anchors, saved viewports), [`tool`] (detail payloads and chip metrics),
//! and [`render`] (all GPUI element building).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    ClipboardItem, Context, Entity, ListAlignment, ListOffset, ListScrollEvent, ListState,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Subscription, Task, Window, px,
};

use holt_doc::{MessageStatus, SessionMessageEntry};

use crate::markdown::parser::{BlockTree, IncrementalParser};
use crate::markdown::render::{RenderCache, update_drag_at};
use crate::markdown::veil::RowVeil;
use crate::motion::{self};
use crate::state::AppState;
use crate::theme::Theme;

mod viewport;

pub use viewport::{
    AT_BOTTOM_PX, FLAVOUR_ROTATE_SECS, FLAVOUR_WORDS, GLIDE_MAX_VIEWPORTS, OVERDRAW_PX,
    SCROLL_BUTTON_THRESHOLD_PX, SPRING_CHASE_MAX_LEAD, SPRING_DAMPING, SPRING_FRAME_MS,
    SPRING_GROWTH_EMA, SPRING_MASS, SPRING_MAX_CATCHUP_FRAMES, SPRING_SETTLE_GRACE_MS,
    SPRING_STIFFNESS, STICK_THRESHOLD_PX, StickSpring, flavour_seed, flavour_word, format_elapsed,
    sending_bridge,
};
use viewport::{
    OWN_SEND_GLIDE_RETAIN, OWN_SEND_GLIDE_SNAP_PX, OWN_SEND_SCROLL_SLACK_PX, OwnTurnAnchor,
    SavedViewport, SavedViewportCache, TranscriptReplayState, own_send_hold_inset,
    own_turn_reservation, should_anchor_live_stream,
};
pub(crate) use viewport::{OWN_SEND_TOP_INSET_PX, SELECTION_SCROLL_TICK_MS, selection_scroll_step};

mod tool;

use tool::blob_detail;
pub use tool::{
    BLOB_AFFORDANCE_HEIGHT, CALL_WRAP_COLS, CHIP_GAP, CHIP_HEIGHT, DIFF_DETAIL_MAX_LINES,
    OUTPUT_DETAIL_MAX_LINES, OUTPUT_LINE_HEIGHT, ToolDetail, call_block, chips_height,
    detail_height, diff_to_file, tool_detail, tool_group_summary,
};

mod markdown;
mod model;

mod approval;
mod plan_card;

pub use approval::{
    VerdictTint, approval_cwd_line, approval_target, pending_approval_gate, pending_approval_tool,
    resolve_approval, verdict_chip, verdict_tint_color,
};

pub use markdown::{ParseOutcome, parse_for_row};
use model::entry_fingerprint;
pub use model::{
    Row, RowKind, ToolItem, UserSkill, diff_rows, format_skill_title, format_timestamp,
    rows_for_entry, top_gap_for, turn_change_row,
};

mod render;

pub use render::{ATT_STRIP_H, ATT_THUMB_H, ATT_THUMB_W, MAX_CONTENT_WIDTH};
use render::{CHANGE_CARD_COLLAPSED_H, HighlightStore};

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

// `single_line` and the per-kind chip label/detail are shared with the terminal
// viewport (`holt_proto::view`): a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use holt_proto::view::{single_line, tool_chip_content};

// ---------------------------------------------------------------------------
// Transcript entity
// ---------------------------------------------------------------------------

struct CachedRows {
    fingerprint: u64,
    rows: Vec<Row>,
}

#[derive(Default, Clone, Copy)]
struct FoldState {
    /// User pin (click); `None` follows the auto-open rule.
    open: Option<bool>,
    /// Bumped per toggle — keys the 200ms height tween.
    epoch: usize,
    /// Height at the moment of the toggle (the tween's start). The destination
    /// is always the *current* target height, so content growth after a toggle
    /// snaps instead of replaying a stale tween.
    from: f32,
    /// When the toggle happened. The tween is armed only for a short window
    /// after the click: gpui replays an element's animation on REMOUNT, and a
    /// virtualized row scrolling back into view is a remount — an armed-forever
    /// tween made every once-collapsed group flash open→closed on each
    /// reappearance (user report).
    toggled_at: Option<Instant>,
    /// The auto-derived open this row last rendered with (`None` until first
    /// sight). An AUTO flip — the streaming tail moving off this group, a
    /// thought losing the tail, the settle — arms the height tween exactly
    /// like a user toggle: the bottom-pinned viewport follows content height
    /// 1:1, so an untweened auto flip reads as a page-wide jump (user report:
    /// jitter while the agent outputs). Tracked in the entity (not element)
    /// so a virtualized remount cannot re-edge.
    auto_open_last: Option<bool>,
    /// Committed analytic body height at the previous render — the start
    /// height an auto-armed group tween lerps from. The committed (not
    /// mid-tween) value, matching what a user toggle's `from` captures.
    last_target: f32,
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<String>,
    /// `Some(doc_id)` pins this instance to a SUBAGENT doc: rows come from
    /// `AppState::sub_transcript(doc_id)` instead of the selected chat, and
    /// the instance is READ-ONLY — no echoes, no own-turn hold, and no global
    /// attachment protection (that set is shared with the primary transcript
    /// and overwritten wholesale).
    doc_override: Option<String>,
    /// Whether an override instance watches a LIVE doc (`for_doc(follow)`):
    /// only then may the working trailer render — a frozen snapshot must
    /// never spin, whatever its entries claim.
    doc_live: bool,
    /// Memory-only viewport state for primary chats visited in this window.
    /// A transcript instance is shared across tabs, so the active ListState is
    /// reset on every attach and cannot retain these positions by itself.
    saved_viewports: SavedViewportCache,
    /// An anchored viewport waiting for the selected chat's async replay.
    pending_viewport: Option<SavedViewport>,
    /// Generation of the selected chat, guarding post-layout restoration
    /// callbacks across rapid A→B→A navigation.
    viewport_generation: u64,
    /// A restored item anchor needs one post-layout refresh of distance-based
    /// UI state; programmatic list scrolling never invokes `handle_scroll`.
    viewport_finalize_pending: bool,
    viewport_finalize_scheduled: bool,
    /// Bumped whenever sync or own-turn logic invalidates measured rows. The
    /// post-restore finalizer waits until one layout completes without another
    /// invalidation, avoiding a stale jump-button decision.
    viewport_layout_revision: u64,
    /// One-shot "open at the latest content" for UNPINNED (frozen) override
    /// instances: rows land ASYNC after the tab opens (watch replay / blob
    /// fetch), so the end-scroll fires on the first non-empty sync, then
    /// never again — landing at the end and FOLLOWING it are different
    /// states, and the user owns the viewport from there. Pinned instances
    /// don't need it (the pin branch already opens at the end).
    land_end_pending: bool,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Detail folds (output/diff) per chip, keyed `"{row_id}#d{ix}"` — full
    /// [`FoldState`]s so detail bodies tween open/closed exactly like the
    /// group fold. Render-local like `folds` — never part of the row
    /// fingerprint.
    tool_details: HashMap<SharedString, FoldState>,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    /// Last UI typography generation reflected in `list` item measurements.
    /// Family and size changes can alter prose wrapping without changing row
    /// identity, so the virtual list must explicitly discard cached heights.
    typography_generation: u32,
    highlights: HighlightStore,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, after an own-send first overflows, and
    /// on the jump button.
    pinned: bool,
    /// A locally-sent prompt currently held near the viewport top while its
    /// reply grows into the empty space below it.
    own_turn: Option<OwnTurnAnchor>,
    /// A layout-affecting change needs one post-layout own-turn measurement.
    own_turn_kick: bool,
    /// One own-turn `on_next_frame` callback in flight at most.
    own_turn_scheduled: bool,
    /// Wall-clock of the previous entry-glide tick (`None` = not gliding).
    own_turn_last_tick: Option<Instant>,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// Last pointer sample while markdown selection owns a left-button drag.
    selection_drag_position: Option<Point<Pixels>>,
    /// One-shot timer rescheduled only while the pointer remains in an edge
    /// zone. Dropping it on mouse-up stops all selection scroll work.
    selection_scroll_task: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Height of the shell's composer/status/terminal stack overlaying the
    /// transcript's bottom (measured last frame): the last row pads past it
    /// so pinned content rests above the glass chrome it scrolls under.
    bottom_clearance: f32,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer — reveals the entry's timestamp
    /// strip (holt chat-view.tsx `group-hover`; the rows report hover
    /// themselves). Keyed by ROW so a row→row move within one entry can't
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Entry whose hover action is showing transient copied-check feedback.
    copied_message: Option<SharedString>,
    copied_message_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<Entity<crate::image_viewer::ImageViewer>>,
    /// Focused while the lightbox is open so Escape reaches it.
    viewer_close_sub: Option<gpui::Subscription>,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(String, String), Task<()>>,
    /// Sidecar blob fetches keyed by doc ref (`chatId/partId[.diff]`,
    /// chat2-sync A3). `Ready` holds the UPGRADED detail, built once on
    /// arrival — render swaps it in per chip; rows never rebuild for it.
    /// Deliberately NOT cleared on chat switch: refs are chat-qualified and a
    /// fetched blob stays valid.
    blob_details: HashMap<SharedString, BlobFetch>,
    /// Monotonic fetch order per blob ref: when a tool has BOTH a diff and
    /// an output blob fetched, the chip shows the one requested most
    /// recently (click "Show full output" after a diff → see the output).
    blob_fetch_order: HashMap<SharedString, u64>,
    blob_fetch_counter: u64,
    /// The plan card's feedback editors (ADR-0025), keyed by plan id —
    /// the plan component's own state, separate from the ADR-0014 gate
    /// notes.
    plan_notes: HashMap<String, ApprovalNote>,
    _observe: Subscription,
}

/// The plan feedback card's expanding note editor (ADR-0025): the input
/// plus its event subscription (Submitted = send the feedback, Edited =
/// repaint).
pub struct ApprovalNote {
    pub input: Entity<crate::composer::ComposerInput>,
    _events: Subscription,
}

/// One sidecar blob fetch's lifecycle.
enum BlobFetch {
    Loading(#[allow(dead_code)] Task<()>),
    /// Failed with the affordance re-armed as a retry.
    Failed,
    Ready(Arc<ToolDetail>),
}

/// Shell-facing events (the transcript itself hosts no surfaces).
#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    /// A spawn chip's "Open subagent" affordance: open the subagent's
    /// transcript as a right-pane tab. `chat_id` is the doc the chip lives
    /// in (the frozen blob is keyed `{chat_id}/{doc_id}`); `frozen` means
    /// the subagent finished — try the blob before watching the doc.
    OpenSubagent {
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
    },
    /// A Turn change card's Review affordance (ADR-0024 ticket 04): open —
    /// or re-aim — the read-only review surface for that Turn. `path`
    /// targets one file when a specific row was clicked; `None` (the card
    /// header's Review) selects the Turn's first file.
    ReviewTurnChanges {
        chat_id: String,
        message_id: String,
        path: Option<String>,
    },
    /// A Turn change card row's Open affordance: the post-Turn file in the
    /// workspace file tab — the path is the change set's repo-relative one.
    /// Deleted files never emit this (the file is gone).
    OpenTurnFile { path: String },
}

impl gpui::EventEmitter<TranscriptEvent> for Transcript {}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        crate::images::observe(cx);
        Self::build(state, None, true, cx)
    }

    /// A read-only transcript over one SUBAGENT doc (right-pane tab). The
    /// caller starts the feed (`watch_subagent_doc` or the frozen snapshot);
    /// this instance only renders whatever lands under `doc_id`. `follow` =
    /// the doc is live: engage the end-follow pin from the start. Either
    /// way the tab OPENS at the latest content — a frozen transcript lands
    /// at the end once, unpinned, and free-scrolls from there.
    pub fn for_doc(
        state: Entity<AppState>,
        doc_id: String,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(state, Some(doc_id), follow, cx)
    }

    fn build(
        state: Entity<AppState>,
        doc_override: Option<String>,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // FollowMode stays Normal: the tail pin is ours (a per-frame spring),
        // not the list's per-layout hard snap.
        //
        // Every transcript aligns TOP (ADR-0008): entries lay out
        // document-style from the pane's top, streaming grows into the empty
        // space below, and a short list rests at the top instead of rising
        // from the bottom. The PIN machinery still runs on top of it for
        // end-follow: the spring is purely distance-based, and the glue trap
        // it was built around is Bottom-only — layout materializes a Top
        // list's past-end offset to a CONCRETE position every frame (gpui
        // list.rs: only `Bottom` re-glues to the `None` sentinel), so a
        // parked spring can't re-glue and hard-track growth. The glue
        // management below (`is_glued`, the −0.75px de-glue,
        // `materialize_scroll_anchor`) is kept but inert under Top.
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW_PX));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        // The rail is sized for the conversation column; a narrow right-pane
        // tab has no width gate driving it, so override instances skip it.
        let rail_enabled = doc_override.is_none();
        // `follow` is the initial pin: the primary transcript always opens
        // pinned; an override instance pins only while its doc is LIVE (a
        // frozen transcript reads top-down, free-scrolling). Short content
        // is at-end by definition (distance 0), so the pin is invisible
        // until streaming overflows the pane — then it follows, releases on
        // wheel-up, and resticks/jumps exactly like the main transcript.
        let pinned = follow;
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            // Pre-set so `sync` never sees an attach edge — an override
            // instance must not reset (or re-pin) on selection changes.
            chat_id: doc_override.clone(),
            land_end_pending: doc_override.is_some() && !follow,
            doc_live: doc_override.is_some() && follow,
            doc_override,
            saved_viewports: SavedViewportCache::default(),
            pending_viewport: None,
            viewport_generation: 0,
            viewport_finalize_pending: false,
            viewport_finalize_scheduled: false,
            viewport_layout_revision: 0,
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            tool_details: HashMap::new(),
            veils: HashMap::new(),
            veil_baseline: std::collections::HashSet::new(),
            veil_attach_pending: true,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            typography_generation: crate::typography::generation(cx),
            highlights: HighlightStore::default(),
            show_jump_button: false,
            last_scroll_distance: 0.0,
            pinned,
            own_turn: None,
            own_turn_kick: false,
            own_turn_scheduled: false,
            own_turn_last_tick: None,
            spring: StickSpring::new(),
            spring_last_tick: None,
            spring_settled_at: None,
            spring_kick: false,
            spring_scheduled: false,
            scroll_anim: None,
            selection_drag_position: None,
            selection_scroll_task: None,
            rail_enabled,
            bottom_clearance: 0.0,
            rail_hover: None,
            hovered_entry: None,
            copied_code: None,
            copied_clear: None,
            copied_message: None,
            copied_message_clear: None,
            attachment_preview: None,
            viewer_close_sub: None,
            attachment_retries: HashMap::new(),
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
            plan_notes: HashMap::new(),
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    // ---- rail plumbing (rendering lives in crate::rail) ----

    /// Shell-driven width gate: the rail hides below 48rem of container width.
    pub fn set_rail_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.rail_enabled != enabled {
            self.rail_enabled = enabled;
            cx.notify();
        }
    }

    pub(crate) fn rail_enabled(&self) -> bool {
        self.rail_enabled
    }

    /// Shell-driven: the measured height of the bottom chrome stack the
    /// transcript scrolls under. Sub-pixel jitter is ignored so steady-state
    /// frames don't re-notify.
    pub fn set_bottom_clearance(&mut self, height: f32, cx: &mut Context<Self>) {
        if (self.bottom_clearance - height).abs() > 0.5 {
            self.bottom_clearance = height;
            if self.own_turn.is_some() {
                self.remeasure_last_row();
                self.own_turn_kick = true;
            }
            cx.notify();
        }
    }

    pub(crate) fn rail_hover(&self) -> Option<usize> {
        self.rail_hover
    }

    pub(crate) fn set_rail_hover(&mut self, hover: Option<usize>) {
        self.rail_hover = hover;
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn list_state(&self) -> &ListState {
        &self.list
    }

    /// Snapshot the outgoing primary chat before its rows and ListState are
    /// reset. Empty rows never overwrite an older snapshot: during a rapid
    /// A→B→A switch, B's replay may not have arrived before leaving it again.
    fn remember_current_viewport(&mut self) {
        // Rows can already contain optimistic echoes while an older snapshot
        // is still waiting for the authoritative replay. Leaving again in
        // that window must preserve the older snapshot, not replace it with
        // the partial echo-only viewport.
        if self.pending_viewport.is_some() {
            return;
        }
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };
        let distance_from_bottom = if self.pinned {
            0.0
        } else {
            self.distance_from_bottom()
        };
        let Some(viewport) = SavedViewport::capture(
            &self.rows,
            self.list.logical_scroll_top(),
            self.pinned,
            distance_from_bottom,
            self.own_turn.as_ref(),
        ) else {
            return;
        };
        self.saved_viewports.insert(chat_id, viewport);
    }

    /// Restore an exact optimistic row while replay is pending, enable stable
    /// fallbacks only after a populated reset, and retire snapshots proven
    /// absent by an empty reset. `scroll_to` remains valid while the virtual
    /// list measures restored rows on the following layout pass.
    fn restore_pending_viewport(&mut self, replay: TranscriptReplayState) -> bool {
        if self.pending_viewport.is_none() {
            return false;
        }
        if !self.rows.is_empty()
            && let Some(restored) = self
                .pending_viewport
                .as_ref()
                .and_then(|saved| saved.resolve(&self.rows, replay.allows_fallback()))
        {
            self.pending_viewport = None;
            self.list.scroll_to(restored.offset);
            self.own_turn = restored.own_turn;
            self.own_turn_kick = self.own_turn.is_some();
            self.own_turn_last_tick = None;
            if self.own_turn.is_some() {
                // Replay readiness can change while echo rows stay identical,
                // so the no-diff path may install a runway without splicing.
                self.remeasure_last_row();
            }
            self.last_scroll_distance = restored.distance_from_bottom;
            self.show_jump_button = restored.distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
            self.viewport_finalize_pending = true;
            return true;
        }

        if !replay.authoritative_empty() {
            return false;
        }
        // The reset's document rows, not the combined rows, define
        // authoritative emptiness. A matching optimistic row above remains
        // valid, but an unrelated echo must never become an index fallback
        // for old history.
        self.discard_pending_viewport();
        if self.own_turn.is_none() {
            self.pinned = true;
            self.last_scroll_distance = 0.0;
            self.show_jump_button = false;
            self.list.scroll_to_end();
        }
        true
    }

    /// Explicit user/navigation intent supersedes a replay-delayed restore.
    /// Replace its cache entry with tail-follow until current rows can be
    /// snapshotted normally on the next chat switch.
    pub(crate) fn discard_pending_viewport(&mut self) {
        if self.pending_viewport.take().is_some()
            && let Some(chat_id) = self.chat_id.clone()
        {
            self.saved_viewports
                .insert(chat_id, SavedViewport::FollowTail);
        }
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Hand viewport ownership to explicit rail/navigation input before its
    /// reduced-motion or animated branch moves the list.
    pub(crate) fn begin_scroll_navigation(&mut self) {
        self.discard_pending_viewport();
        // Rail navigation within the session RELEASES the hold but keeps the
        // runway (user spec: only leaving and revisiting the session clears
        // it) — scrolling back down re-arms the hold like any restick.
        self.release_own_turn_hold();
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
    }

    /// Store the animation after [`Self::begin_scroll_navigation`].
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.scroll_anim = Some(task);
    }

    /// Give the viewport to the user/navigation without dropping the
    /// reservation: the pad stays, the hold stands down until a restick.
    fn release_own_turn_hold(&mut self) {
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.held = false;
        }
        self.own_turn_last_tick = None;
    }

    fn remeasure_last_row(&mut self) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.list.remeasure_items(last..last + 1);
            self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
        }
    }

    pub(crate) fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    /// Whether a user scroll should re-engage the bottom pin: inside the 70px
    /// stick band *and* moving toward the bottom. Direction matters — a small
    /// wheel-up notch near the bottom stays inside the band, and re-sticking
    /// on it would snap the view straight back, making the pin unbreakable.
    pub fn should_restick(distance: f32, previous_distance: f32) -> bool {
        distance <= STICK_THRESHOLD_PX && distance < previous_distance
    }

    fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        // The list invokes this handler ONLY from its wheel/touch input path
        // (programmatic scroll_by/scroll_to never re-enter it), while holding
        // its internal RefCell borrow — reading the ListState back
        // synchronously panics with "already mutably borrowed". Defer to the
        // end of the effect cycle, after the list has released its borrow.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this: &mut Transcript, cx| {
                this.discard_pending_viewport();
                // Wheel/touch while a runway lives: input owns the viewport,
                // and the BOTTOM PIN must stay out of it entirely. Escaping
                // releases the hold (the reservation stays behind as plain
                // scrollable space); returning toward the bottom re-arms the
                // HOLD, never `pinned` — a restick pin glued the view to the
                // bottom of the reservation pad, where streaming reads as
                // text stuck at the viewport top with the runway never
                // filling (user report; the pad can't resize there either,
                // its anchor being off-screen). macOS trackpad momentum can
                // even release-and-restick within one gesture right after a
                // send, so under the old rules the prompt never landed at
                // the top at all.
                if this.own_turn.is_some() {
                    let distance = this.distance_from_bottom();
                    let previous = this.last_scroll_distance;
                    this.last_scroll_distance = distance;
                    let held = this.own_turn.as_ref().is_some_and(|a| a.held);
                    if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                        // Input moving away from the bottom breaks the hold.
                        if let Some(anchor) = this.own_turn.as_mut() {
                            anchor.held = false;
                        }
                        this.own_turn_last_tick = None;
                        this.pinned = false;
                        this.spring.reset();
                        this.spring_last_tick = None;
                    } else if !held
                        && (distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous))
                    {
                        // Returning to the bottom returns to the RUNWAY: the
                        // glide re-lands the prompt at its inset.
                        if let Some(anchor) = this.own_turn.as_mut() {
                            anchor.held = true;
                            anchor.positioned = false;
                        }
                        this.own_turn_last_tick = None;
                        this.own_turn_kick = true;
                    } else if held {
                        // Wheel-down while held: the bottom is a HARD STOP.
                        // The pad runs one frame behind a streaming commit,
                        // so the list's own end-clamp can briefly admit
                        // travel into the transient surplus — re-assert the
                        // hold in the same effect cycle, before anything
                        // paints, and the sink never reaches the screen.
                        // (scroll_to is bounds-free, so this also covers the
                        // wheel gluing the offset at the end.)
                        if let Some(ix) = this.own_turn_anchor_ix() {
                            // Before the hold inset resolves there is no
                            // position to re-assert; the prompt has not
                            // moved yet either.
                            if let Some(inset) = this.own_turn.as_ref().and_then(|a| a.hold_inset) {
                                this.list.scroll_to(ListOffset {
                                    item_ix: ix,
                                    offset_in_item: px(0.0),
                                });
                                this.list.scroll_by(px(-inset));
                            }
                        }
                        this.last_scroll_distance = this.distance_from_bottom();
                    }
                    let show = distance > SCROLL_BUTTON_THRESHOLD_PX
                        && !this.own_turn.as_ref().is_some_and(|a| a.held);
                    if show != this.show_jump_button {
                        this.show_jump_button = show;
                    }
                    cx.notify();
                    return;
                }
                let distance = this.distance_from_bottom();
                let previous = this.last_scroll_distance;
                this.last_scroll_distance = distance;
                if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                    // User input moving away from the bottom breaks the pin.
                    // Content growth never lands here — it doesn't fire the
                    // scroll handler (mugen §1e: interrupt from input, not
                    // scrollbar position).
                    this.pinned = false;
                    this.spring.reset();
                    this.spring_last_tick = None;
                } else if distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous) {
                    // Returning toward the bottom inside the 70px band (or
                    // arriving at it) re-engages the pin with a glide.
                    if !this.pinned {
                        this.pinned = true;
                        this.wake_spring();
                    }
                }
                let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !this.pinned;
                if show != this.show_jump_button {
                    this.show_jump_button = show;
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn on_selection_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() || !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        self.selection_drag_position = Some(event.position);
        if update_drag_at(event.position) {
            cx.notify();
        }
        self.schedule_selection_scroll(cx);
    }

    fn on_selection_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.stop_selection_scroll();
        if let Some(_text) = crate::markdown::selection::end_active_drag() {
            // X11 middle-click paste parity, including the case where the
            // anchor row has virtualized away and cannot receive mouse-up.
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            cx.write_to_primary(ClipboardItem::new_string(_text));
        }
    }

    fn stop_selection_scroll(&mut self) {
        self.selection_drag_position = None;
        self.selection_scroll_task = None;
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_scroll_task.is_some() || !crate::markdown::selection::is_dragging() {
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        if selection_scroll_step(self.list.viewport_bounds(), position) == 0.0 {
            return;
        }
        self.selection_scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECTION_SCROLL_TICK_MS))
                .await;
            let _ = this.update(cx, |transcript, cx| {
                transcript.selection_scroll_task = None;
                transcript.step_selection_scroll(cx);
            });
        }));
    }

    fn step_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        let step = selection_scroll_step(self.list.viewport_bounds(), position);
        if step == 0.0 {
            return;
        }

        // Resolve against the registry painted after the previous step before
        // moving it again. This is what lets a stationary edge pointer consume
        // successive virtualized rows.
        update_drag_at(position);
        self.scroll_anim = None;
        self.discard_pending_viewport();
        self.release_own_turn_hold();
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.list.scroll_by(px(step));
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        cx.notify();
        self.schedule_selection_scroll(cx);
    }

    /// Reserve the reply's space below a locally-sent prompt — EVERY send,
    /// not just the first (a steer or a post-turn send used to collapse the
    /// previous reservation and drop the messages back down — user report).
    /// [`Self::step_own_turn`] sizes the reservation; the motion is just the
    /// bottom pin: with the pad installed, the spring's glide to the new
    /// bottom lands the prompt at the top. Replacing a still-held previous
    /// anchor collapses its pad into the same glide — one continuous motion.
    pub fn on_own_send(&mut self, chat_id: String, message_id: String, cx: &mut Context<Self>) {
        self.discard_pending_viewport();
        self.pinned = false;
        self.show_jump_button = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
        // A glued offset re-snaps to the end on EVERY layout (a gpui
        // Bottom-alignment behavior — inert under Top, kept per ADR-0008) —
        // the pad would land and the viewport hard-track its bottom in the
        // same frame, skipping the glide entirely (rig-traced). Pin the
        // offset to a CONCRETE visible item first; the pad then reads as
        // scrollable distance for the glide to cover.
        self.materialize_scroll_anchor();
        let seen_prompt = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id.as_str());
        self.own_turn = Some(OwnTurnAnchor {
            chat_id,
            message_id: SharedString::from(message_id),
            runway: 0.0,
            hold_inset: None,
            held: true,
            positioned: false,
            seen_prompt,
        });
        self.own_turn_last_tick = None;
        self.own_turn_kick = true;
        self.remeasure_last_row();
        cx.notify();
    }

    /// Convert a glued scroll offset (`None`/past-the-end — under gpui's
    /// Bottom alignment layout re-snaps it to the end each frame; under Top
    /// it materializes to a concrete offset, so this is a no-op) into a
    /// concrete `{item, offset}` anchored at the first visible row, which
    /// layout holds still.
    fn materialize_scroll_anchor(&mut self) {
        if !self.is_glued() {
            return;
        }
        let vp_top = f32::from(self.list.viewport_bounds().top());
        for ix in 0..self.rows.len() {
            if let Some(bounds) = self.list.bounds_for_item(ix)
                && f32::from(bounds.bottom()) > vp_top + 0.5
            {
                self.list.scroll_to(ListOffset {
                    item_ix: ix,
                    offset_in_item: px(vp_top - f32::from(bounds.top())),
                });
                return;
            }
        }
    }

    fn own_turn_anchor_ix(&self) -> Option<usize> {
        let anchor = self.own_turn.as_ref()?;
        self.rows
            .iter()
            .position(|row| row.turn_start && row.entry_id == anchor.message_id)
    }

    fn reconcile_own_turn_prompt(&mut self) {
        let Some(message_id) = self
            .own_turn
            .as_ref()
            .map(|anchor| anchor.message_id.clone())
        else {
            return;
        };
        let exists = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id);
        let keep = self
            .own_turn
            .as_mut()
            .is_some_and(|anchor| anchor.observe_prompt(exists));
        if keep {
            return;
        }

        self.own_turn = None;
        self.own_turn_kick = false;
        self.own_turn_last_tick = None;
        self.remeasure_last_row();
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        self.viewport_finalize_pending = true;
    }

    /// One post-layout own-turn step: size the reservation pad. Pure layout —
    /// all motion is the ordinary bottom pin (see [`OwnTurnAnchor`]).
    fn step_own_turn(&mut self, cx: &mut Context<Self>) {
        self.own_turn_kick = false;
        // Layout moves the bottom too (pad refinement, streaming growth):
        // refresh the wheel handler's escape baseline every frame so only a
        // WHEEL's own delta registers as user intent. Without this, the pad
        // growing at turn-completion between two wheel events read as
        // "scrolled away" and silently released the hold — the next wheels
        // then sank unopposed deep into the runway blank (rig-traced).
        self.last_scroll_distance = self.distance_from_bottom();
        let Some(anchor_ix) = self.own_turn_anchor_ix() else {
            // The optimistic echo may arrive on the next state notification.
            return;
        };
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.seen_prompt = true;
        }
        let viewport = self.list.viewport_bounds();
        let viewport_height = f32::from(viewport.size.height);
        if viewport_height <= 0.0 {
            self.own_turn_kick = true;
            cx.notify();
            return;
        }
        let Some(last_ix) = self.rows.len().checked_sub(1) else {
            return;
        };
        let base_pad = self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0;
        // A glued offset hard-tracks a GROWING end — streamed text visually
        // pushes everything above it up while the runway blank persists
        // below (user report; the glued representation also hides every
        // item's bounds, so the sizing that would consume the runway goes
        // blind). Dissolve it for HELD and RELEASED views alike. The glued
        // sentinel resolves NUMERICALLY to the total content height (a
        // viewport top past the last item), so a small nudge lands in an
        // absurd overscroll that layout's under-fill normalizer re-glues on
        // the very next frame — an invisible wedge loop (rig-traced).
        // Stepping back a FULL viewport from the sentinel is exactly "end
        // at the screen bottom": the same visual position, concrete. All of
        // this is gpui Bottom-alignment behavior: under Top the anchor is
        // concrete by the time this post-layout step runs, so the branch is
        // inert (kept per ADR-0008).
        if self.is_glued() {
            self.list.scroll_by(px(-viewport_height));
        }
        let current = self.own_turn.as_ref().map_or(0.0, |a| a.runway);
        // Resolve the hold inset ONCE, from the anchor row's first measured
        // bounds, and freeze it on the anchor: with reply room below, the
        // prompt holds where it already sits (the first glide tick's err is
        // ~0 and lands immediately); otherwise it glides to the top inset.
        // Missing bounds are NOT "below the fold": `remeasure_items` turns
        // the row Unmeasured until the next layout, and this callback runs
        // before that layout — every `sync` between the previous layout and
        // now (the echo's ack, the session going live) remeasures the last
        // row, i.e. the prompt itself. Freezing the top inset on that
        // transient pulled a mid-screen prompt to the top (user report). A
        // row is only known to be below the fold when the FIRST tick (pad
        // not yet in layout, so the distance is real) sees the content
        // overflow the viewport with the anchor beyond the measured window;
        // otherwise wait a frame — the anchor is on screen and the next
        // layout measures it.
        let inset = match self.own_turn.as_ref().and_then(|a| a.hold_inset) {
            Some(inset) => inset,
            None => {
                let natural_top = self
                    .list
                    .bounds_for_item(anchor_ix)
                    .map(|b| f32::from(b.top()) - f32::from(viewport.top()));
                let below_fold =
                    current <= 0.0 && self.distance_from_bottom() > OWN_SEND_SCROLL_SLACK_PX;
                let resolved = match natural_top {
                    Some(top) => Some(own_send_hold_inset(
                        anchor_ix,
                        top,
                        viewport_height,
                        base_pad,
                    )),
                    None if anchor_ix == 0 => Some(0.0),
                    None if below_fold => Some(OWN_SEND_TOP_INSET_PX),
                    None => None,
                };
                let Some(inset) = resolved else {
                    // Transiently unmeasured. The provisional pad still goes
                    // in now, at the widest sizing (an overshoot is safe —
                    // the refinement below trues it once the inset is
                    // known); the glide and the sizing wait for bounds.
                    if current <= 0.0 {
                        let widest = viewport_height - OWN_SEND_TOP_INSET_PX - base_pad
                            + OWN_SEND_SCROLL_SLACK_PX;
                        if let Some(anchor) = self.own_turn.as_mut() {
                            anchor.runway = widest.max(0.0);
                        }
                        self.remeasure_last_row();
                    }
                    self.own_turn_kick = true;
                    cx.notify();
                    return;
                };
                if let Some(anchor) = self.own_turn.as_mut() {
                    anchor.hold_inset = Some(inset);
                }
                inset
            }
        };
        // The slack keeps the held layout scrollable (see the constant) —
        // the reservation deliberately over-fills by this much.
        let usable = viewport_height - inset - base_pad + OWN_SEND_SCROLL_SLACK_PX;

        // A fresh anchor installs a provisional pad BEFORE anything needs
        // bounds: the just-sent rows sit below the fold, unmeasured, and
        // without the pad there is no scroll room to bring them into the
        // measured window (gating the pad on their bounds deadlocked — the
        // clamped scroll kept them unmeasured forever). Sized at FULL
        // `usable` — a deliberate overshoot by the turn's own height, safe
        // under the absolute hold (scroll_to pins the prompt regardless) and
        // REQUIRED for short chats under the old Bottom alignment: gpui's
        // bottom-aligned list reports no item bounds while its content is
        // shorter than the viewport (rig-traced: a new session's first send
        // sat ~150px below the inset forever — the old undershot pad left
        // the content short, the bounds-free scroll_to clamped, and the
        // bounds-gated refinement could never rescue it). Top-aligned lists
        // measure short content from the top, so bounds exist there; the
        // overshoot stays as kept machinery. Overshooting guarantees the
        // scroll room; the surplus sits below the fold until the refinement
        // trues it.
        if current <= 0.0 {
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.runway = usable.max(0.0);
            }
            self.remeasure_last_row();
            cx.notify();
            return;
        }

        // ---- reservation sizing (skipped while unmeasured: the provisional
        // pad stands; the render gate re-runs this every live frame) --------
        if let (Some(anchor_bounds), Some(last_bounds)) = (
            self.list.bounds_for_item(anchor_ix),
            self.list.bounds_for_item(last_ix),
        ) {
            // Content height of the turn, excluding the pads on the last row.
            let turn_height = f32::from(last_bounds.bottom())
                - f32::from(anchor_bounds.top())
                - current
                - base_pad;
            let target = own_turn_reservation(usable, turn_height);
            // FLOOR: never shrink the pad faster than the viewport allows.
            // The step runs a frame behind content growth, so a wheel that
            // lands inside that window can sink the view toward the stale
            // end; snapping the pad straight to `target` then pulls the end
            // UP THROUGH the viewport (the list clamps instantly — a visible
            // yank, user report "stutter push back"). Shrinking is capped so
            // the end never rises above the current view; deferred surplus
            // burns off as the view moves away from the stop.
            let dist = self.distance_from_bottom();
            let floor = current - (dist - OWN_SEND_SCROLL_SLACK_PX).max(0.0);
            let target = target.max(floor.min(current));
            if target <= 0.5 {
                // The reply has outgrown the reserved space (or the prompt
                // alone overfills it): the pad is ~0, so dropping it is
                // height-neutral. A still-held view hands off to the bottom
                // pin; a released one doesn't move at all.
                let held = self.own_turn.take().is_some_and(|a| a.held);
                self.remeasure_last_row();
                if held {
                    self.engage_pin(cx);
                } else {
                    cx.notify();
                }
                return;
            }
            if (target - current).abs() > 0.5 {
                if let Some(anchor) = self.own_turn.as_mut() {
                    anchor.runway = target;
                }
                // Growth into the reservation shrinks the pad 1:1 — the held
                // layout never moves.
                self.remeasure_last_row();
                cx.notify();
            }
        }

        // ---- entry glide, then absolute hold -------------------------------
        let (held, positioned) = self
            .own_turn
            .as_ref()
            .map_or((false, false), |a| (a.held, a.positioned));
        if !held {
            return;
        }
        if positioned {
            // Landed: re-assert the prompt's position after every layout.
            // scroll_to is absolute and bounds-independent, so neither glue
            // re-snaps, pad-sizing lag, nor a splice's unmeasured flicker can
            // carry the view off the prompt (each broke the spring-held
            // variants of this — rig-traced). ONE-SIDED: only upward drift
            // (view above the hold) is corrected. The scroll slack under the
            // reservation is legal resting space — wheel-down sinks into it
            // and stops hard at the list's own clamp; snapping back up from
            // there made the bottom bounce/stutter on every scroll event
            // (user report). Way-below-slack (impossible short of a bug)
            // still re-asserts.
            let moved = match self.list.bounds_for_item(anchor_ix) {
                Some(b) => {
                    let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                    // The legal rest zone below the hold is the epsilon plus
                    // rounding; anything deeper is a transient-collision sink
                    // and rubber-bands back.
                    !(-(OWN_SEND_SCROLL_SLACK_PX + 2.0)..=0.5).contains(&err)
                }
                // Bounds vanish in the glued representation (dissolved
                // above, so at most for this one frame) and through splice
                // flicker. Near the stop that is dead-band space — no
                // assert (asserting on None here was the bottom bounce);
                // far from it the position is unknowable flicker: re-assert.
                None => self.distance_from_bottom() > OWN_SEND_SCROLL_SLACK_PX + 8.0,
            };
            if moved {
                // Correct with the entry glide's ease, not a snap: the only
                // in-band escapes are one-frame commit transients and splice
                // flicker, and an eased ~200ms return reads as native
                // rubber-banding where an instant re-assert read as stutter
                // (user report). Bounds-less flicker still snaps — there is
                // nothing to ease against.
                match self.list.bounds_for_item(anchor_ix) {
                    Some(b) => {
                        let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                        let now = Instant::now();
                        let frames = match self.own_turn_last_tick {
                            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0
                                / SPRING_FRAME_MS)
                                .min(SPRING_MAX_CATCHUP_FRAMES),
                            None => 1.0,
                        };
                        self.own_turn_last_tick = Some(now);
                        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
                        if err.abs() <= OWN_SEND_GLIDE_SNAP_PX {
                            self.list.scroll_by(px(err));
                            self.own_turn_last_tick = None;
                        } else {
                            self.list.scroll_by(px(err * ease));
                        }
                        self.own_turn_kick = true;
                    }
                    None => {
                        self.list.scroll_to(ListOffset {
                            item_ix: anchor_ix,
                            offset_in_item: px(0.0),
                        });
                        self.list.scroll_by(px(-inset));
                        self.own_turn_last_tick = None;
                    }
                }
                cx.notify();
            } else {
                self.own_turn_last_tick = None;
            }
            return;
        }
        let now = Instant::now();
        let frames = match self.own_turn_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.own_turn_last_tick = Some(now);
        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
        // Remaining travel: the anchor's own error once it measures; the
        // bottom distance while it is still below the measured window (the
        // undershot provisional pad guarantees the bottom stops short of the
        // prompt, so this leg can never overshoot it).
        // The two error legs mean DIFFERENT things at zero: on the bounds
        // leg, err 0 is AT the hold (no correction needed); on the bounds-
        // less leg, err is the distance to the pad's bottom — arrival there
        // still needs the absolute snap onto the anchor (the short-chat/
        // glued landing, where bounds never appear). Conflating them once
        // marked entries "positioned" at the pad bottom without ever
        // landing (rig-caught: sends parked deep in blank runway).
        let (err, anchored) = match self.list.bounds_for_item(anchor_ix) {
            Some(bounds) => (
                f32::from(bounds.top()) - (f32::from(viewport.top()) + inset),
                true,
            ),
            None => (self.distance_from_bottom(), false),
        };
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport_height;
        let err = if err > glide_max {
            self.list.scroll_by(px(err - glide_max));
            glide_max
        } else {
            err
        };
        let land = |list: &ListState| {
            list.scroll_to(ListOffset {
                item_ix: anchor_ix,
                offset_in_item: px(0.0),
            });
            list.scroll_by(px(-inset));
        };
        if motion::reduced_motion(cx) {
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if anchored
            && (-(OWN_SEND_SCROLL_SLACK_PX + 2.0)..=OWN_SEND_GLIDE_SNAP_PX).contains(&err)
        {
            // At the hold — or resting inside the slack under it (a restick
            // that fired at the true bottom): land WITHOUT pulling the view
            // up. Only a still-above position gets the snap.
            if err > 0.5 {
                land(&self.list);
            }
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if !anchored && err <= OWN_SEND_GLIDE_SNAP_PX {
            // Arrived at the bottom with the anchor still unmeasured: the
            // absolute, bounds-free snap IS the landing.
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else {
            self.list.scroll_by(px(err * ease));
        }
        self.own_turn_kick = true;
        cx.notify();
    }

    /// Whether the transcript is currently pinned to the bottom.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the shell should float the "Scroll to bottom" pill (scrolled
    /// more than [`SCROLL_BUTTON_THRESHOLD_PX`] off the end, unpinned).
    pub fn jump_button_shown(&self) -> bool {
        self.show_jump_button
    }

    /// The scroll-to-bottom pill's click: glide back to the end and re-pin.
    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.discard_pending_viewport();
        // With a live runway, "bottom" IS the held position (the reservation
        // makes prompt-at-top and pad-bottom the same place): re-arm the hold
        // and glide back instead of destroying the runway (user spec — only
        // navigating away and back clears it).
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.held = true;
            anchor.positioned = false;
            self.own_turn_last_tick = None;
            self.own_turn_kick = true;
            self.show_jump_button = false;
            cx.notify();
            return;
        }
        self.engage_pin(cx);
    }

    /// Re-engage the bottom pin with a glide. Long jumps teleport to within
    /// [`GLIDE_MAX_VIEWPORTS`] of the end first (mugen `springToBottom`);
    /// reduced motion snaps.
    fn engage_pin(&mut self, cx: &mut Context<Self>) {
        self.pinned = true;
        self.show_jump_button = false;
        if motion::reduced_motion(cx) {
            self.list.scroll_to_end();
            cx.notify();
            return;
        }
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let distance = self.distance_from_bottom();
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
        }
        self.wake_spring();
        cx.notify();
    }

    /// Arm the per-frame spring driver — `render` schedules the next frame
    /// while [`Self::spring_should_run`].
    fn wake_spring(&mut self) {
        self.spring_settled_at = None;
        self.spring_kick = true;
    }

    /// Whether the spring loop needs another frame: off the bottom, carrying
    /// residual motion, or inside the post-landing settle grace.
    fn spring_should_run(&self) -> bool {
        self.spring_kick
            || self.distance_from_bottom() > 0.5
            || !self.spring.is_idle()
            || self.spring_settled_at.is_some()
    }

    /// Whether the scroll offset is in a bottom-glued representation (`None`
    /// or anchored past the end) — states where, under gpui's Bottom
    /// alignment, the next layout hard-snaps to the new end instead of
    /// holding a pixel position. Under the now-universal Top alignment layout
    /// materializes such an anchor to a concrete offset, so this is only
    /// transiently true between a `scroll_to_end` and the next layout (kept
    /// machinery, ADR-0008).
    pub(crate) fn is_glued(&self) -> bool {
        self.list.logical_scroll_top().item_ix >= self.rows.len()
    }

    /// One spring frame: observe target growth, step the stepper, apply the
    /// delta, park after the settle grace. Runs from `window.on_next_frame`,
    /// i.e. after layout — measurements are fresh.
    fn step_spring(&mut self, cx: &mut Context<Self>) {
        self.spring_kick = false;
        if !self.pinned {
            self.spring_last_tick = None;
            return;
        }
        let now = Instant::now();
        let frames = match self.spring_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.spring_last_tick = Some(now);

        let target = f32::from(self.list.max_offset_for_scrollbar().y);
        let mut distance = self.distance_from_bottom();
        // Long jumps (chat switch mid-history, huge pastes) teleport first.
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
            distance = glide_max;
        }
        let pos = target - distance;
        let next = self.spring.step(pos, target, frames);
        if next > pos {
            self.list.scroll_by(px(next - pos));
        }
        self.last_scroll_distance = (target - next).max(0.0);

        if target - next <= 0.5 {
            let settled = *self.spring_settled_at.get_or_insert(now);
            if now.duration_since(settled) >= Duration::from_millis(SPRING_SETTLE_GRACE_MS)
                && self.spring.is_idle()
            {
                // Park: stop scheduling frames until the next wake.
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                return;
            }
        } else {
            self.spring_settled_at = None;
        }
        cx.notify();
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, entries, echoes, replay, turn_change_sets) = {
            let s = self.state.read(cx);
            match &self.doc_override {
                // Pinned to a subagent doc: `selected` equals `chat_id` by
                // construction, so the attach/reset branch below never fires,
                // and echoes stay empty (nothing is ever sent from here).
                // Change-set cards are main-chat Turns only — the map stays
                // empty here, whatever the selected chat holds.
                Some(doc_id) => (
                    Some(doc_id.clone()),
                    s.sub_transcript(doc_id).to_vec(),
                    Vec::new(),
                    TranscriptReplayState::Populated,
                    HashMap::new(),
                ),
                None => {
                    let replay = if !s.transcript_replayed {
                        TranscriptReplayState::Pending
                    } else if s.transcript.is_empty() {
                        TranscriptReplayState::Empty
                    } else {
                        TranscriptReplayState::Populated
                    };
                    (
                        s.selected_chat.clone(),
                        s.transcript.clone(),
                        s.pending_echoes().to_vec(),
                        replay,
                        s.turn_change_sets.clone(),
                    )
                }
            }
        };

        let attached = selected != self.chat_id;
        if attached {
            // Read the incoming snapshot before inserting the outgoing one:
            // a full bounded cache may evict its oldest entry, which can be
            // exactly the chat the user is reopening.
            let saved_viewport = selected
                .as_ref()
                .and_then(|chat_id| self.saved_viewports.get_cloned_and_touch(chat_id));
            self.remember_current_viewport();
            let keep_own_turn = self
                .own_turn
                .as_ref()
                .is_some_and(|anchor| selected.as_deref() == Some(anchor.chat_id.as_str()));
            if !keep_own_turn {
                self.own_turn = None;
                self.own_turn_kick = false;
                self.own_turn_last_tick = None;
            }
            self.chat_id = selected;
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.veils.clear();
            self.render_cache.borrow_mut().clear();
            self.highlights.entries.clear();
            self.copied_message = None;
            self.copied_message_clear = None;
            self.list.reset(0);
            self.pending_viewport = None;
            self.viewport_generation = self.viewport_generation.wrapping_add(1);
            self.viewport_finalize_pending = false;
            if self.own_turn.is_some() {
                // A kept own-turn hold (send-created chat) owns the viewport.
                self.pinned = false;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            } else if let Some(SavedViewport::Anchored {
                anchor,
                distance_from_bottom,
                own_turn,
            }) = saved_viewport
            {
                // Keep a possible runway pending until replay confirms that
                // its optimistic prompt still exists. Installing it on this
                // empty attach frame can leave a failed send's stale anchor
                // intercepting scroll-to-bottom forever.
                self.pinned = false;
                self.last_scroll_distance = distance_from_bottom;
                self.show_jump_button = distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
                self.pending_viewport = Some(SavedViewport::Anchored {
                    anchor,
                    distance_from_bottom,
                    own_turn,
                });
            } else {
                // New chats and chats that were following their tail retain
                // the existing open-at-bottom behavior.
                self.pinned = true;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            }
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
            self.spring_kick = false;
            self.scroll_anim = None;
            self.stop_selection_scroll();
        }

        let mut new_rows: Vec<Row> = Vec::new();
        // The change-set card (ADR-0024 ticket 03) closes its Turn: it lands
        // after the last row of the entries a User message opened — the next
        // User entry (or the transcript's end) ends the Turn. `turn_id`
        // tracks the most recent User entry; the state map guarantees any
        // stored set is non-empty.
        let mut turn_id: Option<&str> = None;
        for (ix, entry) in entries.iter().enumerate() {
            if entry.role == holt_doc::MessageRole::User {
                turn_id = Some(entry.id.as_str());
            }
            new_rows.extend(self.rows_for(entry, false));
            let turn_ends =
                ix + 1 == entries.len() || entries[ix + 1].role == holt_doc::MessageRole::User;
            if turn_ends
                && let Some(id) = turn_id
                && let Some(change_set) = turn_change_sets.get(id)
            {
                new_rows.push(turn_change_row(id, entry.id.clone().into(), change_set));
            }
        }
        for echo in &echoes {
            new_rows.extend(self.rows_for(echo, true));
        }

        // Text already streamed before this (re)attach is the veil BASELINE:
        // its rows' veils seed instead of fading (render creates them from
        // this set), so only post-switch appends animate. Captured from the
        // first NON-EMPTY transcript after attach — the replay frame — never
        // the attach-time sync, whose transcript is still empty (selection
        // clears it; the doc watch refills it async).
        if attached {
            self.veil_baseline.clear();
            self.veil_attach_pending = true;
        }
        if self.veil_attach_pending && !entries.is_empty() {
            self.veil_attach_pending = false;
            self.veil_baseline = new_rows
                .iter()
                .filter(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
                .map(|r| r.id.clone())
                .collect();
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→complete flip (any mid-fade chunk snaps to full, matching the
        // row's version splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });
        self.veil_baseline.retain(|id| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        // Capture this before the row splice changes the list's measured end.
        // When the user is truly live-following, retaining the end anchor
        // keeps the in-flow working trailer at the same viewport position as
        // transcript lines grow above it. Nothing about the trailer's layout
        // or coordinates changes.
        let live_following = should_anchor_live_stream(
            self.pinned,
            self.distance_from_bottom(),
            entries
                .last()
                .is_some_and(|entry| entry.status == Some(MessageStatus::Streaming)),
        );
        let was_empty = self.rows.is_empty();
        let old_last = self.rows.len().checked_sub(1);
        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;

                self.reconcile_own_turn_prompt();
                // Replay readiness is independent of row content: an empty
                // reset (or one identical to optimistic rows) still resolves
                // or retires the pending viewport.
                if self.restore_pending_viewport(replay) {
                    cx.notify();
                }
                return;
            }
            Some((old_range, count)) => {
                // Any replaced row's cached flatten results are stale — and
                // because live replies splice only the rows whose content hash
                // changed (the tail), this is O(changed rows) per commit, never
                // O(reply).
                for row in &self.rows[old_range.clone()] {
                    self.render_cache.borrow_mut().invalidate_row(&row.id);
                }
                if old_range.len() == count {
                    // In-place content change, same row count — notably the
                    // live→complete flip, where EVERY row of the streamed
                    // message changes version (streaming bit, tool auto_open,
                    // timestamp bit) with identical ids. `splice` would reset
                    // those items to hint-less Unmeasured (heights read 0
                    // until the next paint) and, when the viewport-top item is
                    // inside the range, clobber the scroll anchor to the range
                    // start — the end-of-turn up/down jump the spring then has
                    // to walk back. `remeasure_items` keeps old sizes as hints
                    // and holds the anchor across the remeasure.
                    self.list.remeasure_items(old_range);
                } else {
                    self.list.splice(old_range, count);
                }
                self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
            }
        }
        self.rows = new_rows;

        self.reconcile_own_turn_prompt();
        self.restore_pending_viewport(replay);
        if self.land_end_pending && !self.rows.is_empty() {
            // First content for an unpinned override tab: land at the end.
            // `scroll_to_end` is ITEM-anchored (past-the-end offset that the
            // next layout materializes) — a pixel scroll off `max_offset`
            // would land short here, since the freshly-spliced rows are
            // still unmeasured. Short content clamps back to the top under
            // Top alignment, so "end" and "top" coincide there.
            self.land_end_pending = false;
            self.list.scroll_to_end();
        }
        if self.own_turn.is_some() {
            // Appending a reply moves the runway from the previous last row to
            // the new one. Both measurements must be invalidated because the
            // row diff itself only knows that rows were appended at the tail.
            if let Some(old_last) = old_last.filter(|&ix| ix < self.rows.len()) {
                self.list.remeasure_items(old_last..old_last + 1);
            }
            self.remeasure_last_row();
            self.own_turn_kick = true;
        }
        if self.pinned {
            if live_following {
                self.list.scroll_to_end();
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                self.spring_kick = false;
                self.last_scroll_distance = 0.0;
            } else {
                if motion::reduced_motion(cx) || was_empty {
                    // First fill (chat open) lands at the bottom instantly
                    // (mugen initialScroll:'bottom'); reduced motion snaps.
                    self.list.scroll_to_end();
                } else if self.is_glued() {
                    // A glued offset (`None` / anchored past the end) makes
                    // the upcoming layout hard-snap to the new end under
                    // gpui's Bottom alignment — the per-commit stutter.
                    // Materialize a pixel anchor a hair above the bottom so
                    // layout holds position and the spring glides the
                    // growth. Under Top the anchor is already concrete
                    // post-layout, so this only fires in the same effect
                    // cycle as a just-planted `scroll_to_end`, where it
                    // converts the past-end anchor to the equivalent
                    // concrete offset (kept machinery, ADR-0008).
                    self.list.scroll_by(px(-0.75));
                }
                self.spring_kick = true;
            }
        }
        cx.notify();
    }

    /// Cached row build for one entry (streaming entries bypass the cache).
    fn rows_for(&mut self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, pending);
        if !streaming
            && let Some(cached) = self.row_cache.get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            // Render-cache invalidation rides on the row diff in `sync` (only
            // rows whose content hash changed are spliced — the reparsed tail).
            parse_for_row(streaming, key, text, live_parsers, tree_cache).0
        };
        let rows = rows_for_entry(entry, pending, &mut parse);

        if !streaming {
            self.row_cache.insert(
                entry.id.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
        }
        rows
    }

    /// Fetch a sidecar blob (full tool output or diff) and build its upgraded
    /// [`ToolDetail`] once, off the render path. Re-entry while Loading/Ready
    /// is a no-op; Failed re-arms as a retry (the affordance label says so).
    fn spawn_blob_fetch(&mut self, blob_ref: SharedString, cx: &mut Context<Self>) {
        // Rank BEFORE the already-fetched guard: clicking a Ready ref is the
        // "show me this one again" toggle (recency bump + repaint, no
        // re-fetch) — with both a diff and an output fetched, the two
        // affordances must be able to trade places forever.
        self.blob_fetch_counter += 1;
        self.blob_fetch_order
            .insert(blob_ref.clone(), self.blob_fetch_counter);
        match self.blob_details.get(&blob_ref) {
            Some(BlobFetch::Ready(_)) => {
                cx.notify();
                return;
            }
            Some(BlobFetch::Loading(_)) => return,
            Some(BlobFetch::Failed) | None => {}
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let is_diff = blob_ref.ends_with(".diff");
        let ref_key = blob_ref.clone();
        let task = cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": ref_key.as_ref() }),
                Duration::from_secs(20),
            )
            .await;
            let fetched = match reply {
                Ok(value) => {
                    let text = value
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    blob_detail(text, is_diff)
                        .map(|d| BlobFetch::Ready(Arc::new(d)))
                        .unwrap_or(BlobFetch::Failed)
                }
                Err(_) => BlobFetch::Failed,
            };
            this.update(cx, |this, cx| {
                this.blob_details.insert(ref_key, fetched);
                cx.notify();
            })
            .ok();
        });
        self.blob_details.insert(blob_ref, BlobFetch::Loading(task));
    }

    fn toggle_fold(&mut self, row_id: SharedString, open_height: f32, auto_open: bool) {
        let entry = self.folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(auto_open);
        entry.from = if currently_open { open_height } else { 0.0 };
        entry.open = Some(!currently_open);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
    }

    // ---- attachment read-back (user-attachments.tsx + transcript cache) ----

    fn open_image_viewer(
        &mut self,
        targets: Vec<crate::image_viewer::ViewerTarget>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::{AppContext, Focusable};
        let return_focus = window.focused(cx);
        let state = self.state.clone();
        let viewer = cx.new(|cx| crate::image_viewer::ImageViewer::open(state, targets, index, cx));
        window.focus(&viewer.focus_handle(cx), cx);
        self.viewer_close_sub = Some(cx.subscribe_in(
            &viewer,
            window,
            move |this, _, _: &crate::image_viewer::ImageViewerEvent, window, cx| {
                this.attachment_preview = None;
                if let Some(focus) = &return_focus {
                    window.focus(focus, cx);
                }
                cx.notify();
            },
        ));
        self.attachment_preview = Some(viewer);
        cx.notify();
    }

    fn attachment_state(&mut self, path: &str, cx: &mut Context<Self>) -> crate::images::Snapshot {
        if crate::images::begin_load(path) {
            let Some(engine) = self.state.read(cx).engine().cloned() else {
                crate::images::store_error(path, "Engine not connected.");
                return crate::images::snapshot(path);
            };
            let path = path.to_string();
            let load_path = path.clone();
            let executor = cx.background_executor().clone();
            let task = cx.spawn(async move |this, cx| {
                let result = crate::images::load_thumb(&engine, &load_path, &executor).await;
                match result {
                    Ok(thumb) => crate::images::store_loaded(&load_path, thumb),
                    Err(cause) => crate::images::store_error(&load_path, cause),
                }
                this.update(cx, |_, cx| {
                    cx.notify();
                })
                .ok();
            });
            task.detach();
        }
        let snapshot = crate::images::snapshot(path);
        if let crate::images::Snapshot::Error { retry_in, .. } = &snapshot {
            self.schedule_attachment_retry((String::new(), path.to_string()), *retry_in, cx);
        }
        snapshot
    }

    /// One wake-up per errored source: after the backoff elapses, a notify
    /// re-renders the thumb, whose `begin_load` then claims the retry.
    fn schedule_attachment_retry(
        &mut self,
        key: (String, String),
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if delay == Duration::MAX || self.attachment_retries.contains_key(&key) {
            return;
        }
        let wake = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(delay + Duration::from_millis(60))
                .await;
            this.update(cx, |transcript, cx| {
                transcript.attachment_retries.remove(&wake);
                cx.notify();
            })
            .ok();
        });
        self.attachment_retries.insert(key, task);
    }

    /// Toggle a skill chip's fold. Same `folds` map the tool-group
    /// accordions use, keyed by row id. The group's fold heights are
    /// analytic; this body is intrinsic, so a COLLAPSE captures the row's
    /// painted height at click time — the tween then shrinks through every
    /// intermediate height instead of stepping, which is what the stick
    /// spring oscillates on (user report: the page shook on collapse).
    fn toggle_skill_fold(&mut self, row_id: SharedString, cx: &mut Context<Self>) {
        let painted = self
            .rows
            .iter()
            .position(|row| row.id == row_id)
            .and_then(|ix| self.list.bounds_for_item(ix))
            .map(|bounds| f32::from(bounds.size.height));
        let entry = self.folds.entry(row_id).or_default();
        let collapsing = entry.open.unwrap_or(false);
        entry.from = if collapsing {
            painted.unwrap_or(CHIP_HEIGHT)
        } else {
            CHIP_HEIGHT
        };
        entry.open = Some(!collapsing);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
        cx.notify();
    }

    /// Toggle the Turn change card's file list (user request). Unlike the
    /// skill chip the card defaults EXPANDED; the same painted-height
    /// capture drives the collapse tween, so the stick spring never steps.
    fn toggle_change_card_fold(&mut self, row_id: SharedString, cx: &mut Context<Self>) {
        let painted = self
            .rows
            .iter()
            .position(|row| row.id == row_id)
            .and_then(|ix| self.list.bounds_for_item(ix))
            .map(|bounds| f32::from(bounds.size.height));
        let entry = self.folds.entry(row_id).or_default();
        let collapsing = entry.open.unwrap_or(true);
        entry.from = if collapsing {
            painted.unwrap_or(CHANGE_CARD_COLLAPSED_H)
        } else {
            CHANGE_CARD_COLLAPSED_H
        };
        entry.open = Some(!collapsing);
        entry.epoch += 1;
        entry.toggled_at = Some(Instant::now());
        cx.notify();
    }

    /// The working loader, INSIDE the conversation flow: appended under the
    /// last row while the run is live (moved out of the shell's status strip
    /// — user request), so it reads as part of the streaming reply and
    /// scrolls away with it. The spinner drives this entity's frames, which
    /// keeps the elapsed timer ticking through delta-quiet tool runs.
    /// The failed-send retry (trailer affordance): re-kick every delivery
    /// road engine-side (fresh chat2 socket, host nudge, delivery escorts)
    /// and restart the grace clock so the trailer returns to Sending/Queued
    /// while the retry runs.
    fn retry_send(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };
        let engine = self.state.read(cx).engine().cloned();
        self.state.update(cx, |s, cx| {
            s.retry_pending_send(&chat_id, chrono::Utc::now());
            cx.notify();
        });
        if let Some(engine) = engine {
            cx.spawn(async move |_, _| {
                let params = serde_json::json!({ "chatId": chat_id });
                if let Err(err) = engine
                    .client()
                    .call(holt_rpc::methods::RETRY_DELIVERY, params)
                    .await
                {
                    tracing::warn!(error = %err, "delivery retry RPC failed");
                }
            })
            .detach();
        }
    }

    fn copy_message(&mut self, entry_id: SharedString, text: SharedString, cx: &mut Context<Self>) {
        cx.stop_propagation();
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.copied_message = Some(entry_id);
        self.copied_message_clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            this.update(cx, |this, cx| {
                this.copied_message = None;
                this.copied_message_clear = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The change card closes its Turn (ADR-0024 ticket 03): a row lands
    /// after the Turn's last entry — before the NEXT user entry — whenever
    /// the store holds a non-empty set for that Turn's user message, and a
    /// live update resplices only the card row.
    #[gpui::test]
    fn change_cards_close_their_turns_and_render(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, IntoElement as _};
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));

        let turn =
            |id: &str, role: holt_doc::MessageRole, status: Option<holt_doc::MessageStatus>| {
                SessionMessageEntry {
                    id: id.into(),
                    role,
                    parts: vec![holt_doc::MessagePart::Text {
                        id: "t0".into(),
                        text: format!("entry {id}"),
                    }],
                    created_at: 0,
                    device_id: "dev".into(),
                    status,
                    continuation_of: None,
                }
            };
        state.update(cx, |s, cx| {
            s.transcript = vec![
                turn("m-1", holt_doc::MessageRole::User, None),
                turn(
                    "a-1",
                    holt_doc::MessageRole::Assistant,
                    Some(holt_doc::MessageStatus::Complete),
                ),
                turn("m-2", holt_doc::MessageRole::User, None),
                turn(
                    "a-2",
                    holt_doc::MessageRole::Assistant,
                    Some(holt_doc::MessageStatus::Streaming),
                ),
            ];
            s.transcript_replayed = true;
            s.turn_change_sets.insert(
                "m-1".into(),
                holt_proto::TurnChangeSet {
                    chat_id: "chat-1".into(),
                    message_id: "m-1".into(),
                    phase: holt_proto::TurnChangeSetPhase::Final,
                    files: vec![holt_proto::TurnFileChange {
                        path: "src/lib.rs".into(),
                        old_path: None,
                        status: holt_proto::TurnFileChangeStatus::Modified,
                        additions: 3,
                        deletions: 1,
                        binary: false,
                    }],
                    additions: 3,
                    deletions: 1,
                    truncated: false,
                    updated_at: chrono::Utc::now(),
                },
            );
            cx.notify();
        });
        cx.run_until_parked();

        transcript.update(cx, |this, _| {
            let card_ix = this
                .rows
                .iter()
                .position(|row| row.id.as_ref() == "m-1#tcs")
                .expect("the settled Turn's card exists");
            // The card follows the Turn's last entry and precedes the next
            // user message's rows.
            assert_eq!(this.rows[card_ix].entry_id.as_ref(), "a-1");
            assert!(
                this.rows[card_ix + 1].turn_start,
                "the next user entry follows the card"
            );
            // The still-live turn with no changes yet has no card.
            assert!(!this.rows.iter().any(|row| row.id.as_ref() == "m-2#tcs"));
        });

        // Draw the card (header, file row, counts) — a render panic fails
        // the test; the visual review is manual.
        cx.draw(
            gpui::point(gpui::px(0.0), gpui::px(0.0)),
            gpui::size(gpui::px(800.0), gpui::px(600.0)),
            |_, _| transcript.clone().into_any_element(),
        );

        // A live frame for a new Turn inserts exactly its own card row.
        state.update(cx, |s, cx| {
            s.apply_turn_change_set(holt_proto::TurnChangeSet {
                chat_id: "chat-1".into(),
                message_id: "m-2".into(),
                phase: holt_proto::TurnChangeSetPhase::Live,
                files: vec![holt_proto::TurnFileChange {
                    path: "notes.md".into(),
                    old_path: None,
                    status: holt_proto::TurnFileChangeStatus::Added,
                    additions: 5,
                    deletions: 0,
                    binary: false,
                }],
                additions: 5,
                deletions: 0,
                truncated: false,
                updated_at: chrono::Utc::now(),
            });
            cx.notify();
        });
        cx.run_until_parked();
        transcript.update(cx, |this, _| {
            assert!(
                this.rows.iter().any(|row| row.id.as_ref() == "m-2#tcs"),
                "the live Turn's card now shows"
            );
        });
    }

    #[test]
    fn restick_is_direction_aware() {
        // Scrolling away from the bottom never resticks, even inside the band
        // (a 20px wheel notch from the pinned bottom must break the pin).
        assert!(!Transcript::should_restick(20.0, 0.0));
        assert!(!Transcript::should_restick(69.0, 30.0));
        // Returning toward the bottom resticks once inside the 70px band…
        assert!(Transcript::should_restick(69.0, 120.0));
        assert!(Transcript::should_restick(0.0, 30.0));
        // …but not while still outside it.
        assert!(!Transcript::should_restick(200.0, 300.0));
        // No movement — leave the pin alone.
        assert!(!Transcript::should_restick(50.0, 50.0));
    }

    /// The card's Review/Open affordances (ticket 04): a file row emits the
    /// review event for ITS path, the row's Open button emits the open event
    /// without also firing the row's review (stop-propagation), the header's
    /// Review picks no specific path, and a deleted file offers Review only —
    /// no Open affordance exists to click.
    #[gpui::test]
    fn change_card_affordances_emit_review_and_open(cx: &mut gpui::TestAppContext) {
        use std::{cell::RefCell, rc::Rc};

        use gpui::AppContext as _;
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        // The subscription must outlive the clicks — dropping the returned
        // `Subscription` would unsubscribe the sink, leaving it silently empty.
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&transcript, move |_, event: &TranscriptEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
        });

        state.update(cx, |s, cx| {
            s.selected_chat = Some("chat-1".into());
            s.transcript = vec![SessionMessageEntry {
                id: "m-1".into(),
                role: holt_doc::MessageRole::User,
                parts: vec![holt_doc::MessagePart::Text {
                    id: "t0".into(),
                    text: "edit things".into(),
                }],
                created_at: 0,
                device_id: "dev".into(),
                status: None,
                continuation_of: None,
            }];
            s.transcript_replayed = true;
            s.turn_change_sets.insert(
                "m-1".into(),
                holt_proto::TurnChangeSet {
                    chat_id: "chat-1".into(),
                    message_id: "m-1".into(),
                    phase: holt_proto::TurnChangeSetPhase::Final,
                    files: vec![
                        holt_proto::TurnFileChange {
                            path: "a.rs".into(),
                            old_path: None,
                            status: holt_proto::TurnFileChangeStatus::Modified,
                            additions: 1,
                            deletions: 1,
                            binary: false,
                        },
                        holt_proto::TurnFileChange {
                            path: "gone.txt".into(),
                            old_path: None,
                            status: holt_proto::TurnFileChangeStatus::Deleted,
                            additions: 0,
                            deletions: 3,
                            binary: false,
                        },
                    ],
                    additions: 1,
                    deletions: 4,
                    truncated: false,
                    updated_at: chrono::Utc::now(),
                },
            );
            cx.notify();
        });
        cx.run_until_parked();

        // The deleted file row reviews like any other…
        let row = cx
            .debug_bounds("turn-card-file-gone.txt")
            .expect("row drawn");
        cx.simulate_click(row.center(), Default::default());
        assert!(
            matches!(
                events.borrow().last(),
                Some(TranscriptEvent::ReviewTurnChanges { message_id, path, .. })
                    if message_id == "m-1" && path.as_deref() == Some("gone.txt")
            ),
            "{:?}",
            events.borrow()
        );
        // …but it has no Open affordance at all (story 11).
        assert!(cx.debug_bounds("turn-card-open-gone.txt").is_none());

        // A live file's Open emits the open event alone — the row's review
        // does not double-fire through the button.
        let open = cx.debug_bounds("turn-card-open-a.rs").expect("open drawn");
        cx.simulate_click(open.center(), Default::default());
        assert_eq!(events.borrow().len(), 2);
        assert!(
            matches!(
                events.borrow().last(),
                Some(TranscriptEvent::OpenTurnFile { path }) if path == "a.rs"
            ),
            "{:?}",
            events.borrow()
        );

        // The header's Review targets the Turn, not a specific file.
        let header = cx.debug_bounds("turn-card-review").expect("header drawn");
        cx.simulate_click(header.center(), Default::default());
        assert!(
            matches!(
                events.borrow().last(),
                Some(TranscriptEvent::ReviewTurnChanges { chat_id, message_id, path })
                    if chat_id == "chat-1" && message_id == "m-1" && path.is_none()
            ),
            "{:?}",
            events.borrow()
        );
    }

    /// The header toggles the card's file list (user request): default
    /// expanded, a click pins the fold closed, a second click re-expands —
    /// and a Review click inside the header does NOT toggle the fold
    /// (stop-propagation). The closed state's DOM unmount lags the click by
    /// the collapse tween window, so the assertions read the fold state.
    #[gpui::test]
    fn change_card_header_toggles_the_file_list(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        cx.update(|cx| cx.set_global(crate::theme::Theme::default()));
        let state = cx.new(|_| AppState::new());
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));

        state.update(cx, |s, cx| {
            s.selected_chat = Some("chat-1".into());
            s.transcript = vec![SessionMessageEntry {
                id: "m-1".into(),
                role: holt_doc::MessageRole::User,
                parts: vec![holt_doc::MessagePart::Text {
                    id: "t0".into(),
                    text: "edit things".into(),
                }],
                created_at: 0,
                device_id: "dev".into(),
                status: None,
                continuation_of: None,
            }];
            s.transcript_replayed = true;
            s.turn_change_sets.insert(
                "m-1".into(),
                holt_proto::TurnChangeSet {
                    chat_id: "chat-1".into(),
                    message_id: "m-1".into(),
                    phase: holt_proto::TurnChangeSetPhase::Final,
                    files: vec![holt_proto::TurnFileChange {
                        path: "a.rs".into(),
                        old_path: None,
                        status: holt_proto::TurnFileChangeStatus::Modified,
                        additions: 1,
                        deletions: 1,
                        binary: false,
                    }],
                    additions: 1,
                    deletions: 1,
                    truncated: false,
                    updated_at: chrono::Utc::now(),
                },
            );
            cx.notify();
        });
        cx.run_until_parked();

        // Default: expanded, the file row draws, no pin recorded.
        assert!(cx.debug_bounds("turn-card-file-a.rs").is_some());
        transcript.update(cx, |this, _| {
            assert_eq!(this.folds.get("m-1#tcs").and_then(|fold| fold.open), None);
        });

        let bounds = cx.debug_bounds("turn-card-toggle").expect("toggle drawn");
        cx.simulate_click(bounds.center(), Default::default());
        transcript.update(cx, |this, _| {
            assert_eq!(
                this.folds.get("m-1#tcs").and_then(|fold| fold.open),
                Some(false),
                "a header click pins the fold closed"
            );
        });

        // Review lives INSIDE the toggle: its click must not re-open the
        // fold it bubbles through.
        let review = cx.debug_bounds("turn-card-review").expect("review drawn");
        cx.simulate_click(review.center(), Default::default());
        transcript.update(cx, |this, _| {
            assert_eq!(
                this.folds.get("m-1#tcs").and_then(|fold| fold.open),
                Some(false)
            );
        });

        let bounds = cx.debug_bounds("turn-card-toggle").expect("toggle drawn");
        cx.simulate_click(bounds.center(), Default::default());
        transcript.update(cx, |this, _| {
            assert_eq!(
                this.folds.get("m-1#tcs").and_then(|fold| fold.open),
                Some(true)
            );
        });
    }

    #[test]
    fn single_line_collapses_all_whitespace_runs() {
        assert_eq!(single_line("a\nb"), "a b");
        assert_eq!(single_line("  a\t\t b \r\n c  "), "a b c");
        assert_eq!(single_line("plain"), "plain");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("\n\n"), "");
    }
}
