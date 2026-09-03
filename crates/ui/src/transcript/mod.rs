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
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; the first send in an empty chat anchors the prompt at
//! the viewport top and hands off to the same glide when the reply overflows.
//! While that anchor holds, wheel/touch is clamped rather than obeyed — the
//! whole turn is already visible, so there is nothing to scroll to.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, ClipboardItem, Context, Entity, ListAlignment, ListOffset,
    ListScrollEvent, ListState, MouseButton, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels,
    Point, SharedString, StyledImage as _, StyledText, Subscription, Task, TextRun, Window, canvas,
    div, img, list, prelude::*, px, quad,
};

use holt_doc::{MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use holt_proto::ToolCall;

use crate::markdown::parser::{Block, BlockTree, IncrementalParser, InlineRun};
use crate::markdown::render::{self, RenderCache, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;
use holt_syntax::LanguageId as Lang;

mod viewport;

pub(crate) use viewport::OWN_SEND_TOP_INSET_PX;
pub use viewport::{
    AT_BOTTOM_PX, FLAVOUR_ROTATE_SECS, FLAVOUR_WORDS, GLIDE_MAX_VIEWPORTS, OVERDRAW_PX,
    SCROLL_BUTTON_THRESHOLD_PX, SPRING_CHASE_MAX_LEAD, SPRING_DAMPING, SPRING_FRAME_MS,
    SPRING_GROWTH_EMA, SPRING_MASS, SPRING_MAX_CATCHUP_FRAMES, SPRING_SETTLE_GRACE_MS,
    SPRING_STIFFNESS, STICK_THRESHOLD_PX, StickSpring, flavour_seed, flavour_word, format_elapsed,
    sending_bridge,
};
use viewport::{
    OWN_SEND_GLIDE_RETAIN, OWN_SEND_GLIDE_SNAP_PX, OWN_SEND_SCROLL_SLACK_PX, OwnTurnAnchor,
    SELECTION_SCROLL_TICK_MS, SavedViewport, SavedViewportCache, TranscriptReplayState,
    ViewportFinalizeToken, own_turn_reservation, selection_scroll_step, should_anchor_live_stream,
};

mod tool;

pub use tool::{
    BLOB_AFFORDANCE_HEIGHT, CALL_WRAP_COLS, CHIP_GAP, CHIP_HEIGHT, DIFF_DETAIL_MAX_LINES,
    OUTPUT_DETAIL_MAX_LINES, OUTPUT_LINE_HEIGHT, ToolDetail, call_block, chips_height,
    detail_height, diff_to_file, tool_detail, tool_group_summary,
};
use tool::{CHIPS_TOP_PAD, ChipAffordance, blob_detail, format_kb};

mod markdown;
mod model;

pub use markdown::{ParseOutcome, parse_for_row};
pub use model::{
    Row, RowKind, ToolItem, UserSkill, diff_rows, format_skill_title, format_timestamp,
    rows_for_entry, top_gap_for,
};
use model::{
    entry_fingerprint, fnv1a, is_agent_call, is_spawn_link, skill_file_display,
    tool_group_collapses,
};

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Transcript column max width (holt 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;

/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// User-bubble attachment thumbnails (user-attachments.tsx): 112×80 thumbs in
/// a FIXED-height strip (load-state flips never shift the virtualizer).
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;

/// `HOLT_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("HOLT_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

const FRAME_STATS_WINDOW: usize = 240;

/// `HOLT_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("HOLT_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

// `single_line` and the per-kind chip label/detail are shared with the terminal
// viewport (`holt_proto::view`): a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use holt_proto::view::{single_line, tool_chip_content};

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Option<Weak<holt_syntax::HighlightedDocument>>,
    _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
}

impl HighlightStore {
    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<holt_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.as_ref()?;
            if let Some(document) = document.upgrade() {
                return Some(document);
            }
        }
        if let Some(document) = self.cache.get(&document_key) {
            self.entries.insert(
                slot_key,
                HighlightEntry {
                    key: document_key,
                    document: Some(Arc::downgrade(&document)),
                    _task: None,
                },
            );
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move {
                    holt_syntax::highlight(holt_syntax::HighlightRequest {
                        source: &code,
                        path: None,
                        fence_tag: Some(match lang {
                            Lang::Rust => "rust",
                            Lang::JavaScript => "javascript",
                            Lang::Jsx => "jsx",
                            Lang::TypeScript => "typescript",
                            Lang::Tsx => "tsx",
                            Lang::Python => "python",
                            Lang::Go => "go",
                            Lang::Json => "json",
                            Lang::Jsonc => "jsonc",
                            Lang::Bash => "bash",
                            Lang::Toml => "toml",
                            Lang::Markdown => "markdown",
                            Lang::Html => "html",
                            Lang::Css => "css",
                            Lang::Yaml => "yaml",
                            Lang::C => "c",
                            Lang::Cpp => "cpp",
                            Lang::CSharp => "csharp",
                            Lang::Java => "java",
                            Lang::Kotlin => "kotlin",
                            Lang::Swift => "swift",
                            Lang::Ruby => "ruby",
                            Lang::Php => "php",
                            Lang::Sql => "sql",
                            Lang::Lua => "lua",
                            Lang::Dockerfile => "dockerfile",
                            Lang::Nix => "nix",
                            Lang::Make => "make",
                        }),
                    })
                    .ok()
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(document) = document {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key)
                        && entry.key == document_key
                    {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                _task: Some(task),
            },
        );
        None
    }
}

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
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// Focused while the lightbox is open so Escape reaches it.
    attachment_preview_focus: gpui::FocusHandle,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(String, String), Task<()>>,
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
    _observe: Subscription,
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
}

impl gpui::EventEmitter<TranscriptEvent> for Transcript {}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
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
        // Override instances align TOP: a subagent transcript reads like a
        // fresh notes page — entries anchored at the top, streaming growing
        // into the empty space below, never rising from the pane's bottom.
        // Top alignment gets that structurally (a short list rests at the
        // top with no reservation pad), and the PIN machinery still runs on
        // top of it for end-follow: the spring is purely distance-based, and
        // the glue trap it was built around is Bottom-only — layout
        // materializes a Top list's past-end offset to a CONCRETE position
        // every frame (gpui list.rs: only `Bottom` re-glues to the `None`
        // sentinel), so a parked spring can't re-glue and hard-track growth.
        let alignment = if doc_override.is_some() {
            ListAlignment::Top
        } else {
            ListAlignment::Bottom
        };
        let list = ListState::new(0, alignment, px(OVERDRAW_PX));
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
            attachment_preview_focus: cx.focus_handle(),
            attachment_loads: HashMap::new(),
            attachment_retries: HashMap::new(),
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
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
                            this.list.scroll_to(ListOffset {
                                item_ix: ix,
                                offset_in_item: px(0.0),
                            });
                            this.list.scroll_by(px(-Self::own_send_inset(ix)));
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
        if render::update_drag_at(event.position) {
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
        render::update_drag_at(position);
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
        // A glued offset re-snaps to the end on EVERY layout — the pad would
        // land and the viewport hard-track its bottom in the same frame,
        // skipping the glide entirely (rig-traced). Pin the offset to a
        // CONCRETE visible item first; the pad then reads as scrollable
        // distance for the glide to cover.
        self.materialize_scroll_anchor();
        let seen_prompt = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id.as_str());
        self.own_turn = Some(OwnTurnAnchor {
            chat_id,
            message_id: SharedString::from(message_id),
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt,
        });
        self.own_turn_last_tick = None;
        self.own_turn_kick = true;
        self.remeasure_last_row();
        cx.notify();
    }

    /// Convert a glued scroll offset (`None`/past-the-end — layout re-snaps
    /// it to the end each frame) into a concrete `{item, offset}` anchored at
    /// the first visible row, which layout holds still.
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

    /// The held prompt's top offset from the viewport top. Row 0 already
    /// carries the titlebar chrome inside its own box (the first row's
    /// top gap), so the hold adds nothing — adding the inset on top parked
    /// a new chat's first prompt a double-chrome ~66px low (user report).
    fn own_send_inset(anchor_ix: usize) -> f32 {
        if anchor_ix == 0 {
            0.0
        } else {
            OWN_SEND_TOP_INSET_PX
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
        let inset = Self::own_send_inset(anchor_ix);
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
        // at the screen bottom": the same visual position, concrete.
        if self.is_glued() {
            self.list.scroll_by(px(-viewport_height));
        }
        // The slack keeps the held layout scrollable (see the constant) —
        // the reservation deliberately over-fills by this much.
        let usable = viewport_height - inset - base_pad + OWN_SEND_SCROLL_SLACK_PX;
        let current = self.own_turn.as_ref().map_or(0.0, |a| a.runway);

        // A fresh anchor installs a provisional pad BEFORE anything needs
        // bounds: the just-sent rows sit below the fold, unmeasured, and
        // without the pad there is no scroll room to bring them into the
        // measured window (gating the pad on their bounds deadlocked — the
        // clamped scroll kept them unmeasured forever). Sized at FULL
        // `usable` — a deliberate overshoot by the turn's own height, safe
        // under the absolute hold (scroll_to pins the prompt regardless) and
        // REQUIRED for short chats: gpui's bottom-aligned list reports no
        // item bounds while its content is shorter than the viewport
        // (rig-traced: a new session's first send sat ~150px below the
        // inset forever — the old undershot pad left the content short, the
        // bounds-free scroll_to clamped, and the bounds-gated refinement
        // could never rescue it). Overshooting guarantees the scroll room;
        // the surplus sits below the fold until the refinement trues it.
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
    /// or anchored past the end) — states where the next layout hard-snaps to
    /// the new end instead of holding a pixel position.
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
        let (selected, entries, echoes, replay) = {
            let s = self.state.read(cx);
            match &self.doc_override {
                // Pinned to a subagent doc: `selected` equals `chat_id` by
                // construction, so the attach/reset branch below never fires,
                // and echoes stay empty (nothing is ever sent from here).
                Some(doc_id) => (
                    Some(doc_id.clone()),
                    s.sub_transcript(doc_id).to_vec(),
                    Vec::new(),
                    TranscriptReplayState::Populated,
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
        for entry in &entries {
            new_rows.extend(self.rows_for(entry, false));
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
                self.refresh_protected_attachments(cx);
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
        self.refresh_protected_attachments(cx);
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
                    // the upcoming layout hard-snap to the new end — the
                    // per-commit stutter. Materialize a pixel anchor a hair
                    // above the bottom so layout holds position and the
                    // spring glides the growth.
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

    /// Shield the open transcript's attachments from image-cache eviction —
    /// rebuilt on every row sync so a chat switch swaps the set. Without it,
    /// budget pressure evicted thumbnails still on screen (the list caches
    /// rendered rows, so a visible image's LRU tick goes stale).
    fn refresh_protected_attachments(&self, cx: &Context<Self>) {
        // The protected set is GLOBAL and replaced wholesale — an override
        // instance writing it would clobber the primary transcript's keys.
        if self.doc_override.is_some() {
            return;
        }
        let devices = self.attachment_device_ids(cx);
        let mut keys = std::collections::HashSet::new();
        for row in &self.rows {
            if let RowKind::User { attachments, .. } = &row.kind {
                for att in attachments.iter() {
                    for dev in &devices {
                        keys.insert((dev.clone(), att.path.clone()));
                    }
                }
            }
        }
        crate::attachments::protect_attachments(keys);
    }

    /// Devices whose keys may hold a user message's attachment files: the
    /// chat's device (uploads were committed under it) plus this device
    /// (holt's `uniqueIds([attachmentDeviceId, m.device_id])`).
    fn attachment_device_ids(&self, cx: &Context<Self>) -> Vec<String> {
        // `selected_chat_row` belongs to the PRIMARY transcript's chat — an
        // override instance has no chat row, so it claims no devices (its
        // thumbnails degrade to placeholders instead of guessing).
        if self.doc_override.is_some() {
            return Vec::new();
        }
        let state = self.state.read(cx);
        let mut ids = Vec::new();
        if let Some(chat) = state.selected_chat_row() {
            ids.push(chat.device_id.clone());
        }
        if let Some(local) = state.local_device_id.clone()
            && !ids.contains(&local)
        {
            ids.push(local);
        }
        ids
    }

    /// Effective load state for one attachment across its candidate devices:
    /// first Loaded source wins; otherwise loads are (re)claimed and the
    /// snapshot degrades Loading → Error with a scheduled retry wake-up.
    fn attachment_state(
        &mut self,
        device_ids: &[String],
        path: &str,
        cx: &mut Context<Self>,
    ) -> crate::attachments::AttachmentSnapshot {
        use crate::attachments::{AttachmentSnapshot, attachment_snapshot, begin_load};
        for dev in device_ids {
            if let AttachmentSnapshot::Loaded(image) = attachment_snapshot(dev, path) {
                return AttachmentSnapshot::Loaded(image);
            }
        }
        let mut any_loading = false;
        let mut min_retry: Option<Duration> = None;
        for dev in device_ids {
            if begin_load(dev, path) {
                self.spawn_attachment_load(dev.clone(), path.to_string(), cx);
            }
            match attachment_snapshot(dev, path) {
                AttachmentSnapshot::Loaded(image) => return AttachmentSnapshot::Loaded(image),
                AttachmentSnapshot::Loading => any_loading = true,
                AttachmentSnapshot::Error { retry_in } => {
                    min_retry = Some(min_retry.map_or(retry_in, |m| m.min(retry_in)));
                }
            }
        }
        if any_loading {
            return AttachmentSnapshot::Loading;
        }
        match min_retry {
            Some(retry_in) => {
                if let Some(dev) = device_ids.first() {
                    self.schedule_attachment_retry((dev.clone(), path.to_string()), retry_in, cx);
                }
                AttachmentSnapshot::Error { retry_in }
            }
            // No candidate devices at all — the "unavailable" thumb, no retry.
            None => AttachmentSnapshot::Error {
                retry_in: Duration::MAX,
            },
        }
    }

    fn spawn_attachment_load(&mut self, device_id: String, path: String, cx: &mut Context<Self>) {
        use crate::attachments::{read_attachment_image, store_error, store_loaded};
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            store_error(&device_id, &path);
            return;
        };
        let key = (device_id.clone(), path.clone());
        let task = cx.spawn(async move |this, cx| {
            match read_attachment_image(&engine, cx.background_executor(), &path).await {
                Some(loaded) => store_loaded(&device_id, &path, loaded.name.into(), loaded.image),
                None => store_error(&device_id, &path),
            }
            this.update(cx, |transcript, cx| {
                transcript
                    .attachment_loads
                    .remove(&(device_id.clone(), path.clone()));
                cx.notify();
            })
            .ok();
        });
        self.attachment_loads.insert(key, task);
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

    /// The invocation chip that OPENS the agent's reply (seeded by the
    /// engine ahead of any thinking): a flush-left process row in the
    /// tool-chip language — quiet header, thinking-style fold. Expanding
    /// reveals the exact `<skill>` block the model received, with the
    /// source file one click away. Collapsing tweens the measured height
    /// to zero like the tool-group folds.
    fn render_skill_invocation(
        &mut self,
        row_id: &SharedString,
        name: &SharedString,
        file: &SharedString,
        content: &SharedString,
        pending: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        // A fresh COLLAPSE keeps the body mounted for one tween: the wrapper
        // shrinks the measured height to zero over RESIZE (the tool-group
        // fold pattern), then the settled closed state unmounts it.
        let closing = !open
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let formatted_title = format_skill_title(name);
        let toggle_row_id = row_id.clone();
        let header =
            div()
                .id(SharedString::from(format!("{row_id}#skill-toggle")))
                .h(px(CHIP_HEIGHT))
                .w_full()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .when(pending, |el| el.opacity(0.65))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_skill_fold(toggle_row_id.clone(), cx)
                }))
                .child(
                    // The chip rows' content language (chip_header_row): bare
                    // 13px muted icon, medium muted label, hugging truncating
                    // detail, inline disclosure triangle right after it.
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .child(
                            crate::icons::icon(crate::icons::CUBE)
                                .size(px(13.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.85)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .child(SharedString::from(formatted_title)),
                        )
                        .when(!file.is_empty(), |row| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .flex_shrink(1.0)
                                    .overflow_hidden()
                                    .truncate()
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from(skill_file_display(file))),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(SharedString::from(if open { "▾" } else { "▸" })),
                        ),
                );

        let mut column = div().w_full().flex().flex_col().child(header);
        if open || closing {
            let url = format!("file://{}", file.trim_start_matches("file://"));
            let body = div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .mb(px(6.0))
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(8.0))
                        .bg(crate::theme::ink(0.03))
                        .id(SharedString::from(format!("{row_id}#skill-body")))
                        .max_h(px(320.0))
                        .overflow_y_scroll()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(crate::typography::ui_rems(16.0))
                        .text_color(theme.text_muted.opacity(0.9))
                        .child(content.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{row_id}#skill-file")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.opacity(0.75))
                        .on_click(move |_, _, cx| {
                            cx.open_url(&url);
                        })
                        .child(
                            crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                                .size(px(11.0))
                                .text_color(theme.accent.opacity(0.8)),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(10.5))
                                .text_color(theme.accent.opacity(0.8))
                                .child(SharedString::from("Open SKILL.md")),
                        ),
                );
            if open {
                column = column.child(body);
            } else {
                let from = (fold.from - CHIP_HEIGHT).max(0.0);
                column = column.child(div().overflow_hidden().child(body).with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, 0.0, t))),
                ));
            }
        }
        column.into_any_element()
    }

    /// A `/skill` invocation inside the user bubble: the
    /// `[cube icon + Title]` tag with a click through to the source file.
    /// The `<skill>` block itself rides the AGENT entry's opening chip —
    /// this bubble only records what the user did.
    fn render_user_skill(
        &mut self,
        row_id: &SharedString,
        skill: &Arc<UserSkill>,
        text: &SharedString,
        mentions: &Arc<Vec<crate::composer::SentMentionSpan>>,
        theme: &Theme,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_url = (!skill.file.is_empty())
            .then(|| format!("file://{}", skill.file.trim_start_matches("file://")));
        let formatted_title = format_skill_title(&skill.name);
        let skill_tag = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                crate::icons::icon(crate::icons::CUBE)
                    .size(px(16.0))
                    .text_color(theme.accent),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.accent)
                    .child(SharedString::from(formatted_title)),
            );
        let skill_tag: AnyElement = match open_url {
            Some(url) => skill_tag
                .id(SharedString::from(format!("{row_id}#skill-file")))
                .cursor_pointer()
                .hover(|el| el.opacity(0.8))
                .on_click(move |_, _, cx| {
                    cx.open_url(&url);
                })
                .into_any_element(),
            None => skill_tag.into_any_element(),
        };
        if text.is_empty() {
            return skill_tag;
        }
        let multi_line = text.contains('\n');
        div()
            .flex()
            .flex_row()
            .when(multi_line, |el| el.items_start())
            .when(!multi_line, |el| el.items_center())
            .gap(px(8.0))
            .child(skill_tag)
            .child(div().min_w_0().child(user_bubble_text(
                row_id,
                text.clone(),
                mentions.clone(),
                theme,
            )))
            .into_any_element()
    }

    /// The right-aligned thumbnail strip above a user bubble.
    fn render_user_attachments(
        &mut self,
        row_id: &SharedString,
        atts: &[crate::attachments::UserImageAttachment],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::attachments::AttachmentSnapshot;
        let glyph = Theme::of(cx).glyph;
        let device_ids = self.attachment_device_ids(cx);
        let mut strip = div()
            .w_full()
            .h(px(ATT_STRIP_H))
            .flex()
            .flex_row()
            .justify_end()
            .items_start()
            .gap(px(8.0))
            .overflow_hidden()
            .px(px(4.0))
            .pt(px(4.0));
        for (aix, att) in atts.iter().enumerate() {
            let state = self.attachment_state(&device_ids, &att.path, cx);
            // The in-flight send's progress belongs ON the thumbnail
            // (2026-08-18 user request). Two ref shapes mean "still
            // crossing": the queued flow's `pending://` (bytes ship
            // engine-side after the send; the engine rewrites the ref to an
            // absolute path once they land and the run starts) and the
            // legacy echo's synthetic `pending/`. Percent sources, in order:
            // this attachment's own post-send transfer (`WatchTransfers`, by
            // the uploadId its ref names — the leg that actually takes time),
            // else the send-wide staging/legacy upload percent. Neither → the
            // indeterminate spinner (staged-but-waiting, retry backoff, or
            // committed-awaiting-rewrite), so the ring never shows a number
            // that isn't a real transfer position (2026-08-20 report: the
            // staging-only percent blinked out in ~100ms and lied about the
            // slow part).
            let sending = att.path.starts_with("pending://") || att.path.starts_with("pending/");
            let upload_id = att
                .path
                .strip_prefix("pending://")
                .and_then(|rest| rest.split_once('/'))
                .map(|(id, _)| id);
            let uploading = upload_id
                .and_then(|id| self.state.read(cx).transfer_percent(id))
                .or_else(|| {
                    sending
                        .then(|| self.state.read(cx).upload_progress_percent())
                        .flatten()
                });
            let frame = div()
                .flex_none()
                .w(px(ATT_THUMB_W))
                .h(px(ATT_THUMB_H))
                .rounded(px(8.0))
                .overflow_hidden();
            let thumb: AnyElement = match state {
                AttachmentSnapshot::Loaded(image) => {
                    let preview = crate::attachments::PreviewImage {
                        name: image.name.clone(),
                        image: image.image.clone(),
                    };
                    frame
                        .id(SharedString::from(format!("{row_id}#att{aix}")))
                        .relative()
                        .border_1()
                        .border_color(crate::theme::hairline(0.11))
                        .bg(crate::theme::ink(0.035))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.attachment_preview = Some(preview.clone());
                            window.focus(&this.attachment_preview_focus, cx);
                            cx.notify();
                        }))
                        .child(
                            img(image.image.clone())
                                // EXPLICIT dims, not size_full: img layout
                                // honors the intrinsic aspect ratio over a
                                // percent height (gpui f8d8a90 repoint), so
                                // size_full let a tall photo grow past the
                                // frame and the rectangular overflow clip
                                // squared the bottom corners (2026-08-19).
                                .w(px(ATT_THUMB_W - 2.0))
                                .h(px(ATT_THUMB_H - 2.0))
                                // The IMG needs its own radii: the frame's
                                // rounding only clips rectangularly, so the
                                // sprite must round its own corners (7 = the
                                // frame's 8 minus its 1px border).
                                .rounded(px(7.0))
                                .object_fit(ObjectFit::Cover),
                        )
                        .when(sending, |el| {
                            // The pulse read registers this entity for frames,
                            // so the overlay stays live even once the trailer's
                            // 30s pending-send bridge has lapsed.
                            let pulse = motion::pulse_wave(motion::pulse_delta(
                                &motion::HOLT_PULSE,
                                cx.entity_id(),
                                cx,
                            ));
                            let indicator: AnyElement = match uploading {
                                Some(pct) => crate::loaders::upload_progress_ring(pct, 34.0),
                                None => crate::loaders::mini_glyph_spinner(
                                    format!("att-sending-{row_id}-{aix}"),
                                    3.0,
                                    glyph,
                                    cx.entity_id(),
                                    cx,
                                )
                                .into_any_element(),
                            };
                            el.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(7.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.38 + 0.05 * pulse))
                                    .child(indicator),
                            )
                        })
                        .into_any_element()
                }
                // Errored/unavailable: the dashed "missing" thumb.
                AttachmentSnapshot::Error { .. } => frame
                    .border_1()
                    .border_dashed()
                    .border_color(crate::theme::hairline(0.14))
                    .bg(crate::theme::ink(0.025))
                    .into_any_element(),
                // Loading: the pulsing skeleton (same wash as popover skeletons).
                AttachmentSnapshot::Loading => frame
                    .border_1()
                    .border_color(crate::theme::hairline(0.08))
                    .bg(crate::theme::ink(0.055))
                    .opacity(
                        0.35 + 0.4
                            * motion::pulse_wave(motion::pulse_delta(
                                &motion::HOLT_PULSE,
                                cx.entity_id(),
                                cx,
                            )),
                    )
                    .into_any_element(),
            };
            strip = strip.child(thumb);
        }
        strip.into_any_element()
    }

    // ---- rendering ----

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

    fn render_working_trailer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = chrono::Utc::now();
        let (sending, queued, elapsed_secs, seed) = if let Some(doc_id) = &self.doc_override {
            // A subagent doc has no Session row — `indicator_for` would read
            // the PARENT chat's live state into this tab. Liveness rides the
            // doc itself instead: the sink's assistant entry streams until
            // the subagent settles (run teardown finalizes abandoned sinks),
            // and a trailing USER entry is a steer still awaiting its reply
            // segment. Frozen snapshots never spin, whatever they claim.
            if !self.doc_live {
                return None;
            }
            let state = self.state.read(cx);
            let last = state.sub_transcript(doc_id).last()?;
            let live =
                last.status == Some(MessageStatus::Streaming) || last.role == MessageRole::User;
            if !live {
                return None;
            }
            let elapsed = (now.timestamp_millis() - last.created_at).max(0) / 1000;
            (false, false, elapsed, flavour_seed(doc_id))
        } else {
            let chat_id = self.chat_id.clone()?;
            // Failed-send state first: past the grace window the trailer IS
            // the retry affordance, whatever the indicator fell back to.
            if self.state.read(cx).send_undelivered(&chat_id, now) {
                let theme = Theme::of(cx).clone();
                return Some(
                    div()
                        .id("undelivered-retry")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::SPACE_SM))
                        .pt(px(Theme::SPACE_LG))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.retry_send(cx)))
                        .child(SharedString::from("Not delivered — click to retry"))
                        .into_any_element(),
                );
            }
            let (sending, queued, elapsed) = {
                let state = self.state.read(cx);
                if state.indicator_for(&chat_id, now) != crate::state::Indicator::Working {
                    return None;
                }
                // During the send→turn window the session row's `started_at`
                // still belongs to the PREVIOUS turn — a timer based on the
                // send counted the round-trip and then restarted when the
                // turn actually began (user report). Bridge it as "Sending…"
                // with no timer instead; the word + timer start with the
                // turn.
                let turn_started = state.session_for(&chat_id).and_then(|s| s.started_at);
                let sending =
                    sending_bridge(state.pending_send_started(&chat_id, now), turn_started);
                // Degraded delivery path: the send is a durable local write
                // waiting on connectivity — say so instead of faking
                // progress. (The overlay holds while degraded, so this line
                // owns the surface until the ack or the failed state.)
                let queued = sending && state.chat_delivery_degraded(&chat_id);
                let elapsed = turn_started
                    .map(|t| now.signed_duration_since(t).num_seconds().max(0))
                    .unwrap_or(0);
                (sending, queued, elapsed)
            };
            (sending, queued, elapsed, flavour_seed(&chat_id))
        };
        let word = if queued {
            "Queued — will send automatically"
        } else if sending {
            "Sending"
        } else {
            flavour_word(seed, elapsed_secs)
        };
        let theme = Theme::of(cx).clone();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .pt(px(Theme::SPACE_LG))
                .text_size(crate::typography::ui_rems(11.0))
                .child(crate::loaders::gradient_spinner(
                    "working-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(if queued {
                            theme.warning
                        } else {
                            theme.text_muted
                        })
                        .child(SharedString::from(if queued {
                            word.to_string()
                        } else {
                            format!("{word}…")
                        })),
                )
                .when(!sending, |el| {
                    el.child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format_elapsed(elapsed_secs))),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        // The viewport spans the full window (under the titlebar): the first
        // row's gap adds the titlebar's height so a top-scrolled transcript
        // rests below the chrome it fades under. The right pane already pads
        // for the titlebar — an override instance's first row keeps only the
        // ordinary turn gap, or the content sits double-chrome low.
        let top_gap = if ix == 0 {
            if self.doc_override.is_some() {
                Theme::SPACE_LG
            } else {
                Theme::TITLEBAR_HEIGHT + Theme::SPACE_LG + 10.0
            }
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
        };
        // The last row must clear the composer/status stack the transcript
        // scrolls under PLUS the fade band above it, or the timestamp strip
        // (the row's lowest content) renders half-faded (or hidden) when the
        // transcript is pinned to the bottom.
        let bottom_pad = if ix + 1 == self.rows.len() {
            let runway = self
                .own_turn
                .as_ref()
                .filter(|anchor| {
                    self.rows
                        .iter()
                        .any(|candidate| candidate.entry_id == anchor.message_id)
                })
                .map_or(0.0, |anchor| anchor.runway);
            self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0 + runway
        } else {
            0.0
        };
        // Live-run loader rides under the LAST row's content (above its
        // clearance pad), so it sits right beneath the working reply.
        let trailer = (ix + 1 == self.rows.len())
            .then(|| self.render_working_trailer(cx))
            .flatten();

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                text,
                mentions,
                attachments,
                badges,
                skill,
                pending,
            } => {
                let attachments = attachments.clone();
                let badges = badges.clone();
                let text = text.clone();
                let mentions = mentions.clone();
                let skill = skill.clone();
                let pending = *pending;
                // Attachment thumbnails ride ABOVE the bubble, right-aligned
                // (chat-view.tsx RowView: UserAttachmentStrip then the text
                // HStack); image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
                if !attachments.is_empty() {
                    column = column.child(self.render_user_attachments(&row.id, &attachments, cx));
                }
                if !badges.is_empty() {
                    column = column.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap(px(6.0))
                            .pb(px(6.0))
                            .children(badges.iter().enumerate().map(|(bix, badge)| {
                                crate::badges::render(
                                    SharedString::from(format!("{}#badge{bix}", row.id)),
                                    badge,
                                    &theme,
                                )
                            })),
                    );
                }
                if !text.is_empty() || skill.is_some() {
                    let bubble_child = match skill {
                        Some(skill) => {
                            self.render_user_skill(&row.id, &skill, &text, &mentions, &theme, cx)
                        }
                        None => {
                            user_bubble_text(&row.id, text, mentions, &theme).into_any_element()
                        }
                    };

                    // `min_w_0` is load-bearing: gpui text answers min/max-content
                    // probes with its UNWRAPPED width, so without it the bubble's
                    // automatic min-size is the full single-line width — the flex
                    // item can't shrink, `justify_end` pushes the overflow off the
                    // left edge, and long prompts render as one clipped line
                    // instead of wrapping inside the 80% column cap.
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                                .bg(crate::theme::user_bubble_bg())
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_size(crate::typography::ui_rems(14.0))
                                .line_height(crate::typography::ui_rems(22.0))
                                .text_color(theme.text)
                                .when(pending, |el| el.opacity(0.65))
                                .child(bubble_child),
                        ),
                    );
                }
                column.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: None,
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                )
            }
            RowKind::LiveMarkdown { tree, block_ix } => {
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                // Baseline rows (text already streamed when the transcript
                // attached) start seeded: the existing reply must not fade in
                // on a session switch — only fresh appends animate.
                let veil = (!motion::reduced_motion(cx)).then(|| {
                    self.veils
                        .entry(row.id.clone())
                        .or_insert_with(|| {
                            if self.veil_baseline.contains(&row.id) {
                                Rc::new(RefCell::new(RowVeil::seeded()))
                            } else {
                                Rc::default()
                            }
                        })
                        .clone()
                });
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let timer = frame_stats_enabled().then(Instant::now);
                let el = render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|document| document.lines.as_slice()),
                );
                if let Some(start) = timer {
                    record_live_frame_us(start.elapsed().as_micros() as u64);
                }
                // The attach pass for this row is done (every element rendered
                // above seeded its baseline synchronously): elements appearing
                // from the NEXT pass on are newly streamed and fade normally.
                if let Some(veil) = &veil {
                    veil.borrow_mut().finish_seeding();
                }
                // Drive the veil clock: while any chunk is still dissolving,
                // repaint next frame (self-limiting — one callback per frame).
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    let id = cx.entity_id();
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
                el
            }
            RowKind::ToolGroup { tools, auto_open } => {
                self.render_tool_group(&row.id, tools, *auto_open, &theme, cx)
            }
            RowKind::InputChip { header, resolved } => {
                input_chip(header.clone(), *resolved, &theme)
            }
            RowKind::SkillChip {
                name,
                file,
                content,
                pending,
            } => match content {
                // An invocation: the collapsible process row that opens the
                // agent's reply — expand to see the exact `<skill>` block
                // the model received, thinking-style.
                Some(content) => {
                    self.render_skill_invocation(&row.id, name, file, content, *pending, &theme, cx)
                }
                // A read collapse: the compact pointer chip.
                None => skill_chip(name.clone(), file.clone(), *pending, &theme),
            },
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
        };

        // Hover-revealed metadata strip: a RESERVED 32px lane under the
        // entry's last row. Timestamp, copy action, and copied feedback only
        // flip visibility/content, so none of them shifts the virtualizer.
        // User entries align end (under the bubble), assistant entries start.
        // Both read timestamp first, then the copy action.
        let is_user_row = matches!(row.kind, RowKind::User { .. });
        let hovered = self
            .hovered_entry
            .as_ref()
            .is_some_and(|(_, entry)| entry == &row.entry_id);
        let copied_message = self.copied_message.as_ref() == Some(&row.entry_id);
        let copy_text = row.copy_text.clone();
        let copy_entry_id = row.entry_id.clone();
        let strip = row.timestamp.map(|ms| {
            let timestamp = div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted.opacity(0.55))
                .child(SharedString::from(format_timestamp(ms, &chrono::Local)));
            let copy = copy_text.map(|text| {
                let entry_id = copy_entry_id.clone();
                let fade_key = format!("copy-message-hover-{entry_id}");
                div()
                    .id(SharedString::from(format!("copy-message-{entry_id}")))
                    .size(px(Theme::SPACE_MD * 2.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .cursor_pointer()
                    // Same quiet icon-button treatment as the copy action
                    // over transcript code blocks.
                    .bg(motion::hover_blend(
                        &fade_key,
                        gpui::transparent_black(),
                        crate::theme::ink(0.08),
                    ))
                    .on_hover(motion::hover_listener(fade_key))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_message(entry_id.clone(), text.clone(), cx)
                    }))
                    .child(
                        crate::icons::icon(if copied_message {
                            crate::icons::CHECK
                        } else {
                            crate::icons::COPY
                        })
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                    )
            });
            let metadata = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM));
            let metadata = metadata.child(timestamp).children(copy);
            div()
                .h(px(Theme::SPACE_SM + Theme::SPACE_MD * 2.0))
                .pt(px(Theme::SPACE_SM))
                .w_full()
                .flex()
                .items_center()
                // No horizontal inset: the original's `px-1` netted out flush
                // because its message text was inset by the same amount (group
                // padding 4 + inner VStack 4 = 8 = group 4 + px-1 4). Here the
                // markdown text / user bubble sit AT the content column edges,
                // so the label must too — assistant label's left edge on the
                // text's first-character x, user label's right edge on the
                // bubble's right edge (user-reported 4px drift).
                .when(is_user_row, |el| el.justify_end())
                .when(hovered, |el| {
                    el.child(motion::fade_quick(
                        SharedString::from(format!("meta-{}", row.id)),
                        metadata,
                    ))
                })
        });
        let entry_id = row.entry_id.clone();
        let row_id = row.id.clone();
        div()
            .id(row.id.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    let next = Some((row_id.clone(), entry_id.clone()));
                    if this.hovered_entry != next {
                        let entry_changed = this
                            .hovered_entry
                            .as_ref()
                            .is_none_or(|(_, entry)| entry != &entry_id);
                        this.hovered_entry = next;
                        if entry_changed {
                            cx.notify();
                        }
                    }
                } else if this
                    .hovered_entry
                    .as_ref()
                    .is_some_and(|(row, _)| row == &row_id)
                {
                    // Only the row that OWNS the current reveal may clear it —
                    // a stale leave from an earlier row must not blank the
                    // strip the newly entered row just lit.
                    this.hovered_entry = None;
                    cx.notify();
                }
            }))
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (holt `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .child(inner)
                    .children(strip)
                    .children(trailer),
            )
            .into_any_element()
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

    /// Copy-button wiring for one row's code blocks ([`render::CopyUi`]):
    /// click writes the block's code to the clipboard and shows a transient
    /// "Copied" check on that block for ~1.2s (overlay — no layout shift).
    fn copy_ui_for(&self, row_id: &SharedString, cx: &mut Context<Self>) -> render::CopyUi {
        let copied_ix = self
            .copied_code
            .as_ref()
            .filter(|(id, _)| id == row_id)
            .map(|(_, ix)| *ix);
        let row_key = row_id.clone();
        let entity = cx.weak_entity();
        let handler: Rc<render::CopyHandler> = Rc::new(move |ix, code, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
            let row_key = row_key.clone();
            entity
                .update(cx, |this, cx| {
                    this.copied_code = Some((row_key, ix));
                    this.copied_clear = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1200))
                            .await;
                        this.update(cx, |this, cx| {
                            this.copied_code = None;
                            this.copied_clear = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                    cx.notify();
                })
                .ok();
        });
        render::CopyUi { handler, copied_ix }
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<holt_syntax::HighlightedDocument>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language
                    .as_deref()
                    .and_then(holt_syntax::language_for_alias)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    fn tool_diff_highlight_for(
        &mut self,
        row_id: &SharedString,
        tool_ix: usize,
        detail: &ToolDetail,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::changes::DiffHighlights>> {
        let ToolDetail::Diff {
            file,
            old_text,
            new_text,
        } = detail
        else {
            return None;
        };
        let cache_row: SharedString = format!("{row_id}#tool-diff-{tool_ix}").into();
        let old = match old_text {
            Some(source) => {
                let path = file.old_path.as_deref().unwrap_or(&file.path);
                let lang = holt_syntax::language_for_path(path)?;
                Some(
                    self.highlights
                        .request(cache_row.clone(), 0, lang, source, cx)?,
                )
            }
            None => None,
        };
        let new = match new_text {
            Some(source) => {
                let lang = holt_syntax::language_for_path(&file.path)?;
                Some(self.highlights.request(cache_row, 1, lang, source, cx)?)
            }
            None => None,
        };
        Some(Arc::new(crate::changes::DiffHighlights { old, new }))
    }

    fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        auto_open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        // Agent/spawn chips never fold: they are their own row, always open,
        // no "Called N tools" header — a running subagent stays visible.
        let collapses = tool_group_collapses(tools);
        let open = !collapses || fold.open.unwrap_or(auto_open);
        // Chips render their EFFECTIVE detail: the precomputed doc-resident
        // one, upgraded in place by a fetched sidecar blob (chat2-sync A3).
        // Resolved per paint (a HashMap probe per chip) so fetched content
        // needs no row rebuild — arrival is a cx.notify, like a fold toggle.
        let details: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| {
                // Spawn chips never expand — the subagent doc is the record
                // of what the tool did, and an inline body would only repeat
                // it. The whole chip is the "open that doc" click instead.
                if is_spawn_link(tool) {
                    return None;
                }
                // Among fetched blobs, the most recently REQUESTED one wins —
                // a tool can carry both a diff and an output ref, and the
                // user's last click decides which upgrade is showing.
                let mut best: Option<(u64, Arc<ToolDetail>)> = None;
                for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                    if let Some(BlobFetch::Ready(detail)) = self.blob_details.get(blob_ref) {
                        let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                        if best.as_ref().is_none_or(|(o, _)| order > *o) {
                            best = Some((order, detail.clone()));
                        }
                    }
                }
                best.map(|(_, d)| d).or_else(|| tool.detail.clone())
            })
            .collect();
        // Full-invocation blocks — with them, EVERY chip expands: the click
        // always answers "what exactly was this call?", output or not.
        let invocations: Vec<Option<Arc<ToolDetail>>> = tools
            .iter()
            .map(|tool| tool.invocation.clone().filter(|_| !is_spawn_link(tool)))
            .collect();
        // Fetch affordance under each open detail whose full payload is still
        // sidecar-only: `(ref, label)`. Diff offered first (the richer
        // upgrade), then the output — a fetched ref hands the affordance to
        // the NEXT unfetched one instead of retiring it (both must stay
        // reachable when a tool has both).
        let affordances: Vec<Option<ChipAffordance>> = tools
            .iter()
            .map(|tool| {
                // The currently-displayed ref (same recency rule as
                // `details` above): its affordance is spent; any OTHER
                // Ready ref stays offered as a no-fetch toggle.
                let shown: Option<&SharedString> = {
                    let mut best: Option<(u64, &SharedString)> = None;
                    for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                        if matches!(self.blob_details.get(blob_ref), Some(BlobFetch::Ready(_))) {
                            let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                            if best.is_none_or(|(o, _)| order > o) {
                                best = Some((order, blob_ref));
                            }
                        }
                    }
                    best.map(|(_, r)| r)
                };
                let candidates = [
                    (tool.diff_ref.as_ref(), "diff", None),
                    (tool.output_ref.as_ref(), "output", tool.output_bytes),
                ];
                for (blob_ref, what, bytes) in candidates {
                    let Some(blob_ref) = blob_ref else { continue };
                    let label = match self.blob_details.get(blob_ref) {
                        Some(BlobFetch::Ready(_)) => {
                            if shown == Some(blob_ref) {
                                continue;
                            }
                            format!("Show full {what}")
                        }
                        Some(BlobFetch::Loading(_)) => format!("Loading full {what}…"),
                        Some(BlobFetch::Failed) => {
                            format!("Couldn't load full {what} — tap to retry")
                        }
                        None => match bytes {
                            Some(b) => format!("Show full {what} ({})", format_kb(b)),
                            None => format!("Show full {what}"),
                        },
                    };
                    return Some(ChipAffordance {
                        blob_ref: blob_ref.clone(),
                        label: SharedString::from(label),
                    });
                }
                None
            })
            .collect();
        // Which chips have their detail block open (render-local, analytic —
        // the FINAL state; a mid-tween detail already counts as its target).
        let detail_folds: Vec<FoldState> = details
            .iter()
            .zip(&invocations)
            .enumerate()
            .map(|(ix, (detail, invocation))| {
                if detail.is_none() && invocation.is_none() {
                    return FoldState::default();
                }
                self.tool_details
                    .get(&SharedString::from(format!("{row_id}#d{ix}")))
                    .copied()
                    .unwrap_or_default()
            })
            .collect();
        let detail_opens: Vec<bool> = details
            .iter()
            .zip(&invocations)
            .zip(&detail_folds)
            .zip(tools.iter())
            .map(|(((detail, invocation), fold), tool)| {
                // A STREAMING thought chip defaults open (the live thinking
                // is the point); settled chips default closed. A user toggle
                // overrides either way.
                let default_open = tool.is_thought && !tool.resolved;
                (detail.is_some() || invocation.is_some()) && fold.open.unwrap_or(default_open)
            })
            .collect();
        let detail_highlights: Vec<Option<Arc<crate::changes::DiffHighlights>>> = details
            .iter()
            .enumerate()
            .map(|(ix, detail)| {
                detail
                    .as_deref()
                    .filter(|_| detail_opens[ix])
                    .and_then(|detail| self.tool_diff_highlight_for(row_id, ix, detail, cx))
            })
            .collect();
        let open_height = chips_height(tools.len())
            + details
                .iter()
                .zip(&invocations)
                .zip(&affordances)
                .zip(&detail_opens)
                .filter(|(_, open)| **open)
                .map(|(((detail, invocation), affordance), _)| {
                    invocation.as_deref().map_or(0.0, detail_height)
                        + detail.as_deref().map_or(0.0, detail_height)
                        + if affordance.is_some() {
                            BLOB_AFFORDANCE_HEIGHT
                        } else {
                            0.0
                        }
                })
                .sum::<f32>();
        let target = if open { open_height } else { 0.0 };
        let summary = tool_group_summary(tools);

        let toggle_id = row_id.clone();
        // Header (holt tool-group.tsx): a small bare chevron centered over the
        // chips' guide rail, then the quiet 12px summary.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .h(px(CHIP_HEIGHT))
            .cursor_pointer()
            .text_size(px(12.0))
            .line_height(px(18.0))
            // Quiet even when children failed: agents routinely have failed
            // probes mid-work, and a red HEADER read as "this whole step
            // broke" (user report). Failures still show on the individual
            // chips (destructive tint, holt tool-chip.tsx) and in the
            // summary's "· N failed" count.
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), open_height, auto_open);
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
            )
            .child(
                div()
                    .min_w_0()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(tools.iter().enumerate().map(|(ix, tool)| {
                // Spawn chips are LINKS, not accordions: the click opens the
                // subagent's transcript as a right-pane tab (the shell hosts
                // the surface — the chip only announces which doc it indexes).
                if let Some(doc_id) = tool.subagent_ref.clone().filter(|_| is_spawn_link(tool)) {
                    let chat_id = self.chat_id.clone().unwrap_or_default();
                    let title = subagent_tab_title(&tool.call);
                    let frozen = matches!(
                        tool.subagent_status,
                        Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                    );
                    return subagent_chip(
                        tool,
                        SharedString::from(format!("{row_id}#s{ix}")),
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(TranscriptEvent::OpenSubagent {
                                chat_id: chat_id.clone(),
                                doc_id: doc_id.to_string(),
                                title: title.to_string(),
                                frozen,
                            });
                        }),
                        collapses,
                        theme,
                        cx.entity_id(),
                        cx,
                    );
                }
                let detail = details[ix].clone();
                let invocation = invocations[ix].clone();
                if detail.is_none() && invocation.is_none() {
                    return tool_chip(tool, collapses, theme, cx.entity_id(), cx);
                }
                let affordance = affordances[ix].clone();
                let affordance_h = if affordance.is_some() {
                    BLOB_AFFORDANCE_HEIGHT
                } else {
                    0.0
                };
                let open = detail_opens[ix];
                let dfold = detail_folds[ix];
                let key = SharedString::from(format!("{row_id}#d{ix}"));
                // Expandable chip: header row + detail body in ONE flat
                // column (no card chrome) — the guide rail stretches with
                // the row, so an open detail never breaks the rail.
                //
                // The column's height is EXPLICIT, not intrinsic: it is what
                // the open/close tween animates, and it must match the
                // group's analytic height exactly or stacked chips drift
                // (the old bordered card overflowed by its own 2px of
                // borders — user report: "tool calls cut off at the bottom").
                let closed_h = CHIP_HEIGHT;
                let open_h = CHIP_HEIGHT
                    + invocation.as_deref().map_or(0.0, detail_height)
                    + detail.as_deref().map_or(0.0, detail_height)
                    + affordance_h;
                let card_target = if open { open_h } else { closed_h };
                let animating = dfold.epoch > 0
                    && dfold
                        .toggled_at
                        .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
                let toggle_key = key.clone();
                let group_key = row_id.clone();
                let mut card = div()
                    .when(collapses, |el| el.ml(px(12.0)))
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .id(key.clone())
                            .h(px(CHIP_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let entry =
                                    this.tool_details.entry(toggle_key.clone()).or_default();
                                let currently_open = entry.open.unwrap_or(false);
                                entry.from = if currently_open { open_h } else { closed_h };
                                entry.open = Some(!currently_open);
                                entry.epoch += 1;
                                entry.toggled_at = Some(Instant::now());
                                // Arm the GROUP body's height tween too (open
                                // state untouched): the body's height is
                                // analytic over the final detail state, so
                                // without a tween the row snaps to the target
                                // height while the card is still mid-tween —
                                // content below teleported on expand and the
                                // shrinking card clipped on collapse (user
                                // report). `open_height` was computed with
                                // the detail still in its pre-click state,
                                // which is exactly the tween's start; both
                                // tweens share the click instant and the
                                // RESIZE curve, so the row tracks the card's
                                // bottom edge frame-for-frame.
                                let group = this.folds.entry(group_key.clone()).or_default();
                                group.from = open_height;
                                group.epoch += 1;
                                group.toggled_at = Some(Instant::now());
                                cx.notify();
                            }))
                            .child(chip_header(tool, open, theme, cx.entity_id(), cx)),
                    );
                // The body stays mounted while the close tween shrinks over it.
                // Invocation first (what was asked), then output/diff (what
                // came back).
                if open || animating {
                    if let Some(invocation) = invocation.as_deref() {
                        card = card.child(detail_body(invocation, None, theme));
                    }
                    if let Some(detail) = detail.as_deref() {
                        card =
                            card.child(detail_body(detail, detail_highlights[ix].clone(), theme));
                    }
                    if let Some(ChipAffordance { blob_ref, label }) = affordance {
                        let loading = matches!(
                            self.blob_details.get(&blob_ref),
                            Some(BlobFetch::Loading(_))
                        );
                        let mut row = div()
                            .id(SharedString::from(format!("{key}-blob")))
                            .h(px(BLOB_AFFORDANCE_HEIGHT))
                            .flex_none()
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(label);
                        if !loading {
                            row = row
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text_muted))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.spawn_blob_fetch(blob_ref.clone(), cx);
                                    cx.notify();
                                }));
                        }
                        card = card.child(row);
                    }
                }
                let card: AnyElement = if animating {
                    let from = dfold.from;
                    card.with_animation(
                        SharedString::from(format!("{key}-tween{}", dfold.epoch)),
                        RESIZE.animation(),
                        move |el, t| el.h(px(motion::lerp(from, card_target, t))),
                    )
                    .into_any_element()
                } else {
                    card.h(px(card_target)).into_any_element()
                };
                let card = div().min_w_0().flex_1().child(card);
                div()
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_row()
                    // Guide rail: no fixed height — stretches to the card,
                    // detail included. Agent-only groups skip it (no header
                    // chevron for the rail to sit under).
                    .when(collapses, |row| {
                        row.child(
                            div()
                                .ml(px(12.0))
                                .w(px(1.0))
                                .flex_none()
                                .bg(crate::theme::ink(0.08)),
                        )
                    })
                    .child(card)
                    .into_any_element()
            }));

        // Fold body: 200ms committed-height tween on a USER toggle only — and
        // only within a short window of the click. Auto-open (streaming) and
        // content growth never tween, and a SETTLED fold renders at its static
        // height: leaving the tween armed replayed it on every remount, which
        // in a virtualized list means every scroll-back-into-view (only `open`
        // toggles animate — composes with the stick spring). Agent groups skip
        // the fold entirely (always open, no header).
        let animating = collapses
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        let body: AnyElement = if !collapses {
            chips.into_any_element()
        } else if animating {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(chips)
                .with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .h(px(target))
                .child(chips)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            // Tool summaries and cards are code-adjacent chrome. Detail bodies
            // retain their explicit mono/diff typography below this boundary.
            .font_family(theme.font_sans_fixed.clone())
            .when(collapses, |el| el.child(header))
            .child(body)
            .into_any_element()
    }
}

/// A sent message's text with its file-mention chips. The same recipe as the
/// markdown renderer's inline code (`flat_text_element`): chip ranges shape in
/// the mono font at the spectrum's `code_text`, [`StyledText`] supplies wrapped glyph
/// geometry through its layout handle, and a canvas paints the rounded
/// `code_wash` *beneath* the glyphs — so chips wrap, clip, and scroll exactly
/// like the text they decorate.
///
/// Per-frame cost while an assistant message streams below: shaping hits
/// gpui's line-layout cache (identical text + runs ⇒ reuse) and the underlay
/// repaints O(chips) quads — no layout work, no re-projection (spans were
/// computed once in [`rows_for_entry`]).
/// The user bubble's text: runs split at mention-chip boundaries (one plain
/// run when there are none), with the same selection machinery as rendered
/// markdown — the element registers into the frame's document-ordered
/// registry, so drags select, span into adjacent rows, and Cmd+C copies.
fn user_bubble_text(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    theme: &Theme,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, chips read as inline code. Size/line-height flow from the
    // bubble's div like every text child.
    let body_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let chip_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.code_text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::with_capacity(mentions.len() * 2 + 1);
    let mut at = 0;
    for span in mentions.iter() {
        if at < span.range.start {
            runs.push(body_run(span.range.start - at));
        }
        runs.push(chip_run(span.range.len()));
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let wash = theme.code_wash;
    let sel_key: std::sync::Arc<str> = format!("{row_id}:u").into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            for span in mentions.iter() {
                for rect in render::range_rects(&layout, &span.range, 0.0, 2.0) {
                    window.paint_quad(quad(
                        rect,
                        px(5.0),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            render::paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(styled)
        .into_any_element()
}

/// The transcript ErrorChip — a port of holt chat-view.tsx `ErrorChip`
/// (34px-minimum row, `rounded-[10px] border border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message at `text-foreground/80` — a subtle
/// red-tinted wash, never a bare red-stroke box. Unlike the web port, the
/// message WRAPS instead of truncating: startup-crash errors carry the
/// agent's exit status and stderr, and a one-line ellipsis was exactly what
/// made holtsh/holt#95 undiagnosable from the screenshot.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .min_h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .py(px(7.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked — the
/// interactive controls live in the composer (chat-view.tsx `InputChip`):
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// A collapsed skill invocation / skill-file read (ADR-0006): the skill's
/// name in the accent colour with a cube glyph — clicking opens the source
/// `SKILL.md` so the user can inspect exactly what the agent was told to
/// follow. Right-aligned like the user bubble it replaces.
fn skill_chip(name: SharedString, file: SharedString, pending: bool, theme: &Theme) -> AnyElement {
    let open_url =
        (!file.is_empty()).then(|| format!("file://{}", file.trim_start_matches("file://")));
    let clickable_id = (!file.is_empty()).then(|| name.clone());
    let chip = div().py(px(4.0)).w_full().flex().justify_end().child(
        div()
            .min_h(px(34.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .overflow_hidden()
            .rounded(px(10.0))
            .bg(crate::theme::ink(0.06))
            .px(px(12.0))
            .text_size(px(13.0))
            .when(pending, |el| el.opacity(0.65))
            .child(
                crate::icons::icon(crate::icons::CUBE)
                    .size(px(15.0))
                    .text_color(theme.accent),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.accent)
                    .child(name),
            ),
    );
    match (clickable_id, open_url) {
        (Some(id), Some(url)) => chip
            .id(id)
            .cursor_pointer()
            .hover(|el| el.opacity(0.8))
            .on_click(move |_, _, cx| {
                cx.open_url(&url);
            })
            .into_any_element(),
        _ => chip.into_any_element(),
    }
}

/// A small glyph standing in for the tool's icon (holt uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (holt tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        call if is_agent_call(call) => crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// The body of an expanded chip card, under the header's separator. Diffs
/// render through the changes pane's section body — the real component, with
/// hunk headers, dual line-number gutters, accent bars, row washes, and
/// syntax runs — so an inline tool diff is indistinguishable from the
/// checkout diff sidebar. Output renders as a code block: verbatim mono
/// lines, indentation intact, counted-tail truncation.
fn detail_body(
    detail: &ToolDetail,
    diff_highlights: Option<Arc<crate::changes::DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let body = div().w_full().min_w_0().flex().flex_col().overflow_hidden();
    match detail {
        // No comment layer: an inline tool diff is a record of what the
        // agent already did, not a review surface.
        ToolDetail::Diff { file, .. } => body
            .child(crate::changes::render_file_body_with_syntax(
                file,
                diff_highlights,
                theme,
            ))
            .into_any_element(),
        ToolDetail::Stats { stats } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(stats.iter().map(|stat| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text.opacity(0.85))
                            .child(SharedString::from(stat.path.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.success)
                            .child(SharedString::from(format!("+{}", stat.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("−{}", stat.deletions))),
                    )
            }))
            .into_any_element(),
        ToolDetail::Output {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(lines.iter().map(|line| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .text_color(theme.text.opacity(0.85))
                    .child(div().w_full().min_w_0().truncate().child(line.clone()))
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .text_size(px(12.0))
            .children(lines.iter().map(|line| {
                let row = div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center();
                let Some((text, runs)) = thought_line_text(line, theme) else {
                    return row; // blank separator row
                };
                row.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .child(StyledText::new(text).with_runs(runs)),
                )
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
    }
}

/// The counted-tail row under a truncated Output/Thought detail.
fn more_lines_row(truncated_by: usize, theme: &Theme) -> gpui::Div {
    div()
        .h(px(OUTPUT_LINE_HEIGHT))
        .px(px(12.0))
        .flex()
        .items_center()
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(format!("… {truncated_by} more lines")))
}

/// Shape one flattened thought line into gpui text runs — the detail-body
/// palette: extra-muted foreground prose (a thought is context, dimmer than
/// the reply around it), semibold for bold, violet mono for code, underlined
/// links (NOT clickable — a thought is a record, not a surface).
fn thought_line_text(line: &[InlineRun], theme: &Theme) -> Option<(SharedString, Vec<TextRun>)> {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    for run in line {
        if run.text.is_empty() {
            continue;
        }
        let mut f = if run.style.code {
            gpui::font(theme.font_mono.clone())
        } else {
            gpui::font(theme.font_sans.clone())
        };
        if run.style.bold {
            f.weight = gpui::FontWeight::SEMIBOLD;
        }
        if run.style.italic {
            f.style = gpui::FontStyle::Italic;
        }
        runs.push(TextRun {
            len: run.text.len(),
            font: f,
            color: if run.style.code {
                render::inline_code_text(theme)
            } else {
                theme.text.opacity(0.6)
            },
            background_color: None,
            underline: run.style.link.is_some().then_some(gpui::UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
        text.push_str(&run.text);
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((text.into(), runs))
}

/// The inline trailing affordance on a chip header, when it has one.
enum ChipTrail {
    /// Expand/collapse chevron — flipped while the detail body is open.
    Chevron { open: bool },
    /// Top-right "opens elsewhere" arrow — the spawn chip's link to its
    /// subagent tab.
    OpenArrow,
}

/// The chip's content row: bare icon + label + detail line (+ inline trailing
/// affordance when the chip expands or links out). Shared between the plain
/// chip, the header of an expandable chip, and the spawn link chip. Flat by
/// design — no card chrome; the row is the chip.
///
/// Spawn chips carry their subagent's lifecycle VISUALLY, in the chip's own
/// language: while running the mini working spinner (the sidebar's) pulses
/// at the right of the ordinary static detail; done is the ordinary quiet
/// chip; failed takes the danger tint — no status words, no live text (a
/// header rewriting itself per stream delta read as noise — user report).
fn chip_header_row(
    tool: &ToolItem,
    trail: Option<ChipTrail>,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content(&tool.call)
    };
    let running = tool.subagent_ref.is_some()
        && matches!(tool.subagent_status, Some(SubagentStatus::Running));
    // An ordinary tool mid-call (part not yet resolved): same trailing
    // spinner the subagent chip gets, so "what is it doing" is visible
    // without opening anything.
    let pending = !tool.is_thought && !tool.resolved && tool.subagent_ref.is_none();
    let failed = tool.is_error
        || (tool.subagent_ref.is_some()
            && matches!(tool.subagent_status, Some(SubagentStatus::Failed)));
    let tint = if failed {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .text_size(px(12.0))
        .line_height(px(18.0))
        .child(
            // Bare leading icon — the slot keeps the old tile's 18px
            // footprint so the guide rail hanging under it doesn't move.
            div()
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(if tool.is_thought {
                        crate::icons::CHAT_ROUND_LINE
                    } else {
                        tool_icon_path(&tool.call)
                    })
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.85)),
                ),
        )
        .child(
            div()
                .flex_none()
                .h(px(18.0))
                .flex()
                .items_center()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tint)
                .child(SharedString::from(label)),
        )
        // The detail hugs its text (grow 0, shrink 1) so the trailing
        // affordance sits right after it; a long line still truncates.
        .when(!detail.is_empty(), |row| {
            row.child(
                div()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .truncate()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.text.opacity(0.85)
                    })
                    .child(SharedString::from(detail)),
            )
        })
        .when_some(tool.call.subagent_model(), |row, model| {
            // Which model the child runs on, when the spawn named one.
            //
            // In the trailing slot rather than suffixed onto the detail: the
            // detail is the truncating slot, and the model is exactly what a
            // reader scanning a fan-out of spawns wants left once the
            // descriptions are cut.
            //
            // Bare faint text, NOT a filled pill: the affordances either side
            // of it (spinner = running, arrow = opens the subagent) already
            // claim the trailing edge; giving a passive label the same chrome
            // made the row read as three buttons — the loudest thing in the
            // row was the one thing you cannot click.
            row.child(
                div()
                    .flex_none()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(model.to_owned())),
            )
        })
        .when(running, |row| {
            // The sidebar working-row spinner, in the chip's trailing slot —
            // paint-local (fixed footprint), so it never moves the layout.
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                format!(
                    "subagent-chip-{}",
                    tool.subagent_ref.as_deref().unwrap_or_default()
                ),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when(pending, |row| {
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                SharedString::from(format!(
                    "tool-chip-{}",
                    fnv1a(format!("{:?}", tool.call).as_bytes())
                )),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when_some(trail, |row, trail| {
            // Inline trailing affordance (no tile): a chevron for the
            // output/diff accordion, or the open-arrow for spawn chips.
            row.child(match trail {
                ChipTrail::Chevron { open } => div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
                ChipTrail::OpenArrow => div().flex_none().child(
                    crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.8)),
                ),
            })
        })
}

/// The header row of an expandable chip card.
fn chip_header(
    tool: &ToolItem,
    open: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    chip_header_row(tool, Some(ChipTrail::Chevron { open }), theme, view, cx)
}

/// Max chars a subagent tab title keeps. The strip chip is fixed-width and
/// truncates visually, but the derived title also rides drag ghosts and any
/// future pickers — cap it at the source.
const SUBAGENT_TITLE_MAX: usize = 40;

/// First line of `text`, trimmed, capped at `max` chars with an ellipsis.
fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// Drop a leading "Agent"/"Task" genus (with its `:` and spacing) from a
/// spawn-title candidate. Only a real word boundary strips — "Taskmaster"
/// keeps its name. A bare "Agent"/"Task" strips to "" (no context at all).
fn strip_spawn_prefix(text: &str) -> &str {
    let t = text.trim();
    for prefix in ["agent", "task"] {
        if t.len() >= prefix.len()
            && t.is_char_boundary(prefix.len())
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &t[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    t
}

/// Tab title for a spawn chip's subagent surface: the BARE task description
/// ("verify the marker pipeline"). The chip keeps the tool's fuller name —
/// a fixed-width tab spent on "Agent: " never shows the task, so the genus
/// is stripped here and the call input's description/prompt fields back up
/// a bare name (older docs); "Subagent" only as the last resort.
fn subagent_tab_title(call: &ToolCall) -> SharedString {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Subagent".into(),
    };
    let candidates = [
        Some(name),
        input.and_then(|i| i.get("description")?.as_str()),
        input.and_then(|i| i.get("prompt")?.as_str()),
    ];
    for text in candidates.into_iter().flatten() {
        if let Some(title) = title_line(strip_spawn_prefix(text), SUBAGENT_TITLE_MAX) {
            return title.into();
        }
    }
    "Subagent".into()
}

/// A plain (non-expandable) chip: a flat quiet row, plus the group guide rail
/// when the chip lives under a collapsible header.
fn tool_chip(
    tool: &ToolItem,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .when(rail, |el| el.ml(px(12.0)))
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .child(chip_header_row(tool, None, theme, view, cx)),
        )
        .into_any_element()
}

/// A spawn chip: same flat row as [`tool_chip`], but the WHOLE row is the
/// "open the subagent tab" click (open-arrow affordance in the trailing
/// slot). No accordion — an inline body would only repeat the subagent's own
/// transcript. The group guide rail is omitted for agent-only rows (no
/// collapse header for it to hang from).
fn subagent_chip(
    tool: &ToolItem,
    id: SharedString,
    on_open: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .id(id)
                .when(rail, |el| el.ml(px(12.0)))
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .rounded(px(6.0))
                .cursor_pointer()
                // Invisible until hover — the flat row carries no chrome, the
                // wash is pure clickability feedback.
                .hover(|s| s.bg(crate::theme::ink(0.04)))
                .on_click(on_open)
                .child(chip_header_row(
                    tool,
                    Some(ChipTrail::OpenArrow),
                    theme,
                    view,
                    cx,
                )),
        )
        .into_any_element()
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let typography_generation = crate::typography::generation(cx);
        if self.typography_generation != typography_generation {
            self.typography_generation = typography_generation;
            // `refresh_windows` re-lays out visible rows, but ListState keeps
            // measured heights for virtualized rows outside the viewport.
            // Mark every row unmeasured while retaining height hints and a
            // proportional scroll anchor; GPUI will refresh each measurement
            // as the row enters its layout range.
            self.list.remeasure();
        }
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::attachments::flush_evicted(Some(window), cx);
        // Own-turn driver: measurements are only authoritative after layout,
        // so reservation sizing, the send glide, and the outgrown-handoff
        // each advance at most once per requested frame. Scheduled on every
        // frame while an anchor is live (not just on kicks) so viewport
        // resizes and streaming growth re-derive the reservation; the step
        // only notifies on change, so a settled hold schedules no next frame.
        if (self.own_turn.is_some() || self.own_turn_kick) && !self.own_turn_scheduled {
            self.own_turn_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.own_turn_scheduled = false;
                        this.step_own_turn(cx);
                    })
                    .ok();
            });
        }
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        // Programmatic `scroll_to` does not invoke the list's user-scroll
        // handler. Refresh distance-derived state once layout has measured the
        // replay, guarded so a stale A callback cannot mutate B (or a newer A).
        if self.viewport_finalize_pending && !self.viewport_finalize_scheduled {
            self.viewport_finalize_scheduled = true;
            let token = ViewportFinalizeToken {
                generation: self.viewport_generation,
                layout_revision: self.viewport_layout_revision,
            };
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.viewport_finalize_scheduled = false;
                        if !token.still_current(this.viewport_generation) {
                            if this.viewport_finalize_pending {
                                cx.notify();
                            }
                            return;
                        }
                        let distance = this.distance_from_bottom();
                        this.last_scroll_distance = distance;
                        this.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX
                            && !this.pinned
                            && !this.own_turn.as_ref().is_some_and(|turn| turn.held);
                        if token.layout_settled(this.viewport_layout_revision) {
                            this.viewport_finalize_pending = false;
                        }
                        cx.notify();
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        let list_el = list(self.list.clone(), cx.processor(Self::render_row))
            .size_full()
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let content: AnyElement = if self.doc_override.is_some() {
            // The primary transcript's fade lives on the SHELL's outlet
            // wrapper (it spans the titlebar/composer chrome); an override
            // instance owns its own — top edge only (nothing overlays the
            // pane's bottom), gated on real overflow so a short top-anchored
            // transcript shows no fade. Gated here rather than at paint via
            // a ScrollHandle (the list isn't one); scrolls re-render this
            // entity, so the flag can't go stale.
            let scrolled_under_top = {
                let max = f32::from(self.list.max_offset_for_scrollbar().y);
                max - self.distance_from_bottom() > 1.0
            };
            crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                scrolled_under_top,
                false,
                list_el,
            )
            .into_any_element()
        } else {
            list_el.into_any_element()
        };
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::on_selection_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            .child(content)
            .child(rail);
        // Full-size viewer for a clicked user-bubble thumbnail
        // (AttachmentPreviewDialog: bare lightbox, click closes).
        if let Some(preview) = self.attachment_preview.clone() {
            let weak = cx.weak_entity();
            return root.child(crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                &self.attachment_preview_focus,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.attachment_preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn subagent_tab_titles() {
        // The tab is the BARE task — the "Agent:" genus is stripped.
        let named = ToolCall::Unknown {
            name: "Agent: scan repo".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&named).as_ref(), "scan repo");
        // A bare "Task"/"Agent" digs the description out of the call input
        // (which sheds any genus of its own).
        let bare = ToolCall::Unknown {
            name: "Task".into(),
            input: Some(serde_json::json!({
                "description": "Agent: audit the auth flow",
                "prompt": "very long instructions…",
            })),
        };
        assert_eq!(subagent_tab_title(&bare).as_ref(), "audit the auth flow");
        // Word boundaries only — a name that merely STARTS with the genus
        // keeps itself.
        let compound = ToolCall::Unknown {
            name: "Taskmaster".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&compound).as_ref(), "Taskmaster");
        // Nothing to derive → the generic label.
        let blank = ToolCall::Unknown {
            name: "agent".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&blank).as_ref(), "Subagent");
        // Absurd lengths cap with an ellipsis; multiline prompts keep only
        // their first line.
        let long = ToolCall::Unknown {
            name: "x".repeat(120),
            input: None,
        };
        let title = subagent_tab_title(&long);
        assert_eq!(title.chars().count(), SUBAGENT_TITLE_MAX + 1);
        assert!(title.ends_with('…'));
        // Non-spawn-shaped calls stay generic.
        assert_eq!(
            subagent_tab_title(&ToolCall::Exec {
                command: "ls".into()
            })
            .as_ref(),
            "Subagent"
        );
    }

    #[test]
    fn tool_chip_labels_per_kind() {
        assert_eq!(
            tool_chip_content(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("Run", "cargo test".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }),
            ("Search", "foo in src".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::ApplyPatch { path: None }),
            ("Patch", "workspace".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
        let todo = ToolCall::Todo {
            items: vec![
                holt_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                holt_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
    }

    #[test]
    fn multiline_command_flattens_to_one_chip_line() {
        // The user's breaker: a multi-line script in a Run chip. The detail
        // must come out as ONE sanitized line — the chip's fixed-height row
        // then truncates it with an ellipsis like the original's CSS.
        let (label, detail) = tool_chip_content(&ToolCall::Exec {
            command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
        });
        assert_eq!(label, "Run");
        assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
        assert!(!detail.contains('\n'));
        // The chip row height is a constant, independent of content shape.
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        // Every detail kind is sanitized (MCP inputs / queries are model text).
        let (_, q) = tool_chip_content(&ToolCall::WebSearch {
            query: "line one\nline two".into(),
        });
        assert_eq!(q, "line one line two");
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
