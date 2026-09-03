//! Pure viewport state for the transcript: the stick-to-bottom spring,
//! selection edge-scrolling, own-send anchoring, saved/restore viewport
//! bookkeeping, and the working-indicator flavour vocabulary. No GPUI
//! rendering lives here — heights and offsets only.

use std::collections::{HashMap, VecDeque};

use gpui::{Bounds, ListOffset, Pixels, Point, SharedString, px};

use super::Row;
use super::model::fnv1a;
use crate::theme::Theme;
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
pub const OVERDRAW_PX: f32 = 320.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Bound session-local viewport memory independently of total chat history.
const MAX_SAVED_VIEWPORTS: usize = 256;
/// Text-selection edge scrolling runs only during a drag. A 24 ms cadence is
/// smooth enough to track text while avoiding a permanent animation-frame loop
/// on low-end devices.
pub(super) const SELECTION_SCROLL_TICK_MS: u64 = 24;
const SELECTION_SCROLL_EDGE_PX: f32 = 36.0;
const SELECTION_SCROLL_MAX_STEP_PX: f32 = 24.0;

/// Signed list scroll step for a pointer near a viewport edge.
///
/// GPUI list offsets increase toward the document bottom. The quadratic ramp
/// keeps entry into the edge zone gentle and reaches full speed at the edge.
pub(super) fn selection_scroll_step(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
    let height = f32::from(bounds.size.height);
    if height <= 0.0 {
        return 0.0;
    }
    let edge = SELECTION_SCROLL_EDGE_PX.min(height / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let y = f32::from(position.y);
    let top = f32::from(bounds.top());
    let bottom = f32::from(bounds.bottom());
    let scaled = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        SELECTION_SCROLL_MAX_STEP_PX * t * t
    };
    if y < top + edge {
        -scaled(top + edge - y)
    } else if y > bottom - edge {
        scaled(y - (bottom - edge))
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Stick-to-bottom spring (mugen §1e — same constants as its DEFAULT_SPRING,
// which follows the shape of stackblitz/use-stick-to-bottom)
// ---------------------------------------------------------------------------

/// Retains velocity frame-to-frame (higher = more glide).
pub const SPRING_DAMPING: f32 = 0.7;
/// Pull toward the target (higher = snappier).
pub const SPRING_STIFFNESS: f32 = 0.05;
/// Inertia (higher = slower to start/stop).
pub const SPRING_MASS: f32 = 1.25;
/// Reference frame for the fixed-timestep integration (60fps).
pub const SPRING_FRAME_MS: f32 = 1000.0 / 60.0;
/// Cap on simulated frames per tick — a hitch catches up instead of teleporting.
pub const SPRING_MAX_CATCHUP_FRAMES: f32 = 8.0;
/// EMA rate for the feed-forward target-growth estimate.
pub const SPRING_GROWTH_EMA: f32 = 0.12;
/// While streaming, chase up to this many px above the true bottom (keeps the
/// growing tail visible instead of hugging a moving edge).
pub const SPRING_CHASE_MAX_LEAD: f32 = 32.0;
/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;

/// A live stream already resting at the end should keep that end anchored as
/// its measured height grows. This is deliberately narrower than `pinned`:
/// users gliding back toward the bottom keep the normal spring behavior.
pub(super) fn should_anchor_live_stream(
    pinned: bool,
    distance_from_bottom: f32,
    streaming: bool,
) -> bool {
    pinned && streaming && distance_from_bottom <= AT_BOTTOM_PX
}

/// Keep the spring loop warm this long after landing, so a streaming pause
/// resumes at cruise instead of re-accelerating from zero.
pub const SPRING_SETTLE_GRACE_MS: u64 = 500;
/// Teleport when farther than this many viewports from the end; glide the rest.
pub const GLIDE_MAX_VIEWPORTS: f32 = 2.5;
/// A freshly-sent prompt rests this far below the transcript viewport's top.
/// The titlebar overlays the full-height list, so its height is part of the
/// inset; the extra 10px matches the first row's breathing room.
pub(crate) const OWN_SEND_TOP_INSET_PX: f32 = Theme::TITLEBAR_HEIGHT + 10.0;
/// Epsilon of extra height under the reservation. The runway ends AT the
/// app's bottom — this is not scroll room (24px of it read as a janky
/// overshoot-and-fight zone, user report) — it exists only to keep the held
/// layout out of gpui's shorter-than-viewport regime, where a bottom-aligned
/// list reports no item bounds (sizing goes blind) and position becomes a
/// function of content height instead of the hold. Two pixels of travel is
/// below perception. That regime was gpui Bottom-alignment behavior; under
/// the now-universal Top alignment short content still measures, so the
/// slack is retained but no longer load-bearing (ADR-0008).
pub(super) const OWN_SEND_SCROLL_SLACK_PX: f32 = 2.0;
/// Per-60fps-frame fraction of the remaining entry glide retained (~90%
/// covered in ~230ms, ease-out).
pub(super) const OWN_SEND_GLIDE_RETAIN: f32 = 0.85;
/// The entry glide snaps to the absolute hold within this error.
pub(super) const OWN_SEND_GLIDE_SNAP_PX: f32 = 1.0;

/// The reservation a held turn still needs: the room under the prompt's
/// top-inset position (`usable` = viewport minus inset and bottom chrome)
/// not yet consumed by the turn's own content. Zero once the reply has
/// filled the reserved space — the notes-app `minHeight` analogue.
pub(super) fn own_turn_reservation(usable: f32, turn_height: f32) -> f32 {
    (usable - turn_height).max(0.0)
}

/// Pure stick-to-bottom spring stepper — the mugen `tick()` integration:
/// velocity relaxes toward `(damping·v + stiffness·diff)/mass` per 60fps
/// sub-frame, position advances by `v + target_vel` where `target_vel` is a
/// feed-forward EMA of target growth px/frame, and the chase point sits up to
/// [`SPRING_CHASE_MAX_LEAD`] px above the true bottom proportional to growth.
#[derive(Debug, Clone, Copy)]
pub struct StickSpring {
    /// Spring velocity, px per 60fps frame.
    velocity: f32,
    /// Feed-forward: smoothed target growth, px per 60fps frame.
    target_vel: f32,
    /// Target observed at the previous tick (`None` = fresh/parked).
    last_target: Option<f32>,
}

impl Default for StickSpring {
    fn default() -> Self {
        Self::new()
    }
}

impl StickSpring {
    pub fn new() -> Self {
        Self {
            velocity: 0.0,
            target_vel: 0.0,
            last_target: None,
        }
    }

    /// Park the spring (drops all state; the next tick starts cold).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Residual motion below mugen's settle thresholds (`v < .05 && targetVel
    /// < .05`)?
    pub fn is_idle(&self) -> bool {
        self.velocity < 0.05 && self.target_vel < 0.05
    }

    #[cfg(test)]
    pub(crate) fn target_vel(&self) -> f32 {
        self.target_vel
    }

    /// Advance one tick. `pos`/`target` are scroll offsets in px (larger =
    /// closer to the bottom); `frames` is elapsed time in 60fps frames
    /// (clamped by the caller to [`SPRING_MAX_CATCHUP_FRAMES`]). Returns the
    /// new position: never overshoots `target`, monotone while approaching,
    /// and snaps exactly once within 0.5px.
    pub fn step(&mut self, mut pos: f32, target: f32, mut frames: f32) -> f32 {
        let grew = self.last_target.map_or(0.0, |last| target - last);
        self.last_target = Some(target);
        if grew < -1.0 {
            // Target shrank (row collapse/removal) — growth estimate is stale.
            self.target_vel = 0.0;
        } else {
            let observed = grew.max(0.0) / frames.max(0.25);
            self.target_vel += SPRING_GROWTH_EMA * (observed - self.target_vel);
        }
        let chase = target - (self.target_vel * 9.0).min(SPRING_CHASE_MAX_LEAD);
        let mut v = self.velocity;
        while frames > 0.0 {
            let h = frames.min(1.0);
            frames -= h;
            let diff = (chase - pos).max(0.0);
            v += h * ((SPRING_DAMPING * v + SPRING_STIFFNESS * diff) / SPRING_MASS - v);
            pos = (pos + (v + self.target_vel) * h).min(target);
        }
        self.velocity = v;
        if target - pos <= 0.5 { target } else { pos }
    }
}

// ---------------------------------------------------------------------------
// Working indicator flavour (pure; rendered by the shell strip)
// ---------------------------------------------------------------------------

/// Rotating flavour vocabulary (20 words / 7s, seeded per chat).
pub const FLAVOUR_WORDS: [&str; 20] = [
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The flavour word for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// The working trailer's "Sending…" bridge: true while an in-flight send is
/// fresher than the session row's turn start — the row still carries the
/// PREVIOUS turn (or none), so a timer would count the send round-trip and
/// restart when the turn actually begins.
pub fn sending_bridge(
    send_started: Option<chrono::DateTime<chrono::Utc>>,
    turn_started: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    match (send_started, turn_started) {
        (Some(send), Some(turn)) => turn <= send,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Layout state for the most recent locally-sent turn (notes-app parity):
/// EVERY send reserves the space below the prompt for the reply — a trailing
/// runway pad sized `usable − turn height`, i.e. a min-height for the turn,
/// shrinking 1:1 as the reply streams so the held layout never moves. The
/// entry is an eased glide onto the prompt; landed, the hold re-asserts the
/// prompt's position absolutely after every layout (the bottom spring can't
/// hold here: parking at exact distance 0 re-glues a bottom-aligned gpui
/// list, which then hard-tracks the pad's stale bottom on every commit —
/// rig-traced; re-glue is Bottom-only, so under the now-universal Top
/// alignment the absolute hold is belt-and-braces, ADR-0008). Wheel
/// input releases the hold, leaving the reservation as plain scrollable
/// space. The anchor retires once the reply overflows the reservation (pad
/// ~0, height-neutral). Chat switches snapshot its runway with the viewport
/// and restore it released, so revisiting never resumes hidden auto-follow.
#[derive(Clone, Debug)]
pub(super) struct OwnTurnAnchor {
    pub(super) chat_id: String,
    pub(super) message_id: SharedString,
    /// Current reservation pad on the last row (`usable − turn_height`).
    pub(super) runway: f32,
    /// The step still owns the viewport (glide → hold). Any wheel/touch
    /// input releases it — the reservation stays behind as plain scrollable
    /// space, and the ordinary escape/restick rules apply from then on.
    pub(super) held: bool,
    /// The entry glide has landed; the hold now re-asserts the prompt's
    /// position absolutely after every layout (glue- and lag-proof — the
    /// exact mechanism the shipped first-send anchor used).
    pub(super) positioned: bool,
    /// A fresh send may install the anchor one notification before its echo.
    /// Once the prompt has appeared, its later disappearance is terminal
    /// (failed echo or removed entry) and the runway must retire.
    pub(super) seen_prompt: bool,
}

impl OwnTurnAnchor {
    pub(super) fn released_for_restore(mut self) -> Self {
        self.held = false;
        self.positioned = false;
        self.seen_prompt = true;
        self
    }

    pub(super) fn observe_prompt(&mut self, exists: bool) -> bool {
        if exists {
            self.seen_prompt = true;
        }
        exists || !self.seen_prompt
    }
}

/// A stable per-chat viewport anchor. Row identity is preferred over its old
/// index because async replay can insert or remove rows while a chat is away.
#[derive(Clone, Debug)]
pub(super) struct ViewportAnchor {
    row_id: SharedString,
    entry_id: SharedString,
    fallback_ix: usize,
    offset_in_row: Pixels,
}

impl ViewportAnchor {
    pub(super) fn capture(rows: &[Row], scroll_top: ListOffset) -> Option<Self> {
        let fallback_ix = scroll_top.item_ix.min(rows.len().checked_sub(1)?);
        let row = &rows[fallback_ix];
        Some(Self {
            row_id: row.id.clone(),
            entry_id: row.entry_id.clone(),
            fallback_ix,
            offset_in_row: scroll_top.offset_in_item,
        })
    }

    pub(super) fn resolve_exact(&self, rows: &[Row]) -> Option<ListOffset> {
        let item_ix = rows.iter().position(|row| row.id == self.row_id)?;
        Some(ListOffset {
            item_ix,
            offset_in_item: self.offset_in_row,
        })
    }

    pub(super) fn resolve(&self, rows: &[Row]) -> Option<ListOffset> {
        if let Some(offset) = self.resolve_exact(rows) {
            return Some(offset);
        }

        // A row can disappear when a streaming block is reshaped. Stay in the
        // same message entry, choosing the surviving row nearest the old
        // location; the intra-row offset is no longer meaningful in that case.
        let item_ix = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry_id == self.entry_id)
            .min_by_key(|(ix, _)| ix.abs_diff(self.fallback_ix))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| self.fallback_ix.min(rows.len().saturating_sub(1)));
        (!rows.is_empty()).then_some(ListOffset {
            item_ix,
            offset_in_item: px(0.0),
        })
    }
}

/// Session-local viewport state. Chats that were following their tail keep
/// following it; only user-owned viewports restore a concrete row anchor.
#[derive(Clone, Debug)]
pub(super) enum SavedViewport {
    FollowTail,
    Anchored {
        anchor: ViewportAnchor,
        distance_from_bottom: f32,
        /// Preserve the runway that made a short active turn scrollable.
        /// Navigation releases its automatic hold, so revisiting restores the
        /// viewport without immediately following new output to the bottom.
        own_turn: Option<OwnTurnAnchor>,
    },
}

pub(super) struct RestoredViewport {
    pub(super) offset: ListOffset,
    pub(super) distance_from_bottom: f32,
    pub(super) own_turn: Option<OwnTurnAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ViewportFinalizeToken {
    pub(super) generation: u64,
    pub(super) layout_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TranscriptReplayState {
    Pending,
    Empty,
    Populated,
}

impl TranscriptReplayState {
    pub(super) fn authoritative_empty(self) -> bool {
        self == Self::Empty
    }

    pub(super) fn allows_fallback(self) -> bool {
        self == Self::Populated
    }
}

impl ViewportFinalizeToken {
    pub(super) fn still_current(self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(super) fn layout_settled(self, layout_revision: u64) -> bool {
        self.layout_revision == layout_revision
    }
}

impl SavedViewport {
    pub(super) fn capture(
        rows: &[Row],
        scroll_top: ListOffset,
        pinned: bool,
        distance_from_bottom: f32,
        own_turn: Option<&OwnTurnAnchor>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if pinned {
            return Some(Self::FollowTail);
        }
        Some(Self::Anchored {
            anchor: ViewportAnchor::capture(rows, scroll_top)?,
            distance_from_bottom,
            own_turn: own_turn.cloned(),
        })
    }

    /// Before the opening reset arrives, rows may contain only optimistic
    /// echoes. In that gap an exact row is safe, but entry/index fallbacks
    /// would mistake an unrelated echo for the authoritative transcript.
    pub(super) fn resolve(&self, rows: &[Row], allow_fallback: bool) -> Option<RestoredViewport> {
        let Self::Anchored {
            anchor,
            distance_from_bottom,
            own_turn,
        } = self
        else {
            return None;
        };
        let offset = if allow_fallback {
            anchor.resolve(rows)?
        } else {
            anchor.resolve_exact(rows)?
        };
        let own_turn = own_turn
            .clone()
            .filter(|turn| {
                rows.iter()
                    .any(|row| row.turn_start && row.entry_id == turn.message_id)
            })
            .map(OwnTurnAnchor::released_for_restore);
        Some(RestoredViewport {
            offset,
            distance_from_bottom: *distance_from_bottom,
            own_turn,
        })
    }
}

#[derive(Default)]
pub(super) struct SavedViewportCache {
    by_chat: HashMap<String, SavedViewport>,
    recency: VecDeque<String>,
}

impl SavedViewportCache {
    pub(super) fn insert(&mut self, chat_id: String, viewport: SavedViewport) {
        if self.by_chat.contains_key(&chat_id) {
            self.recency.retain(|candidate| candidate != &chat_id);
        }
        self.recency.push_back(chat_id.clone());
        self.by_chat.insert(chat_id, viewport);
        while self.by_chat.len() > MAX_SAVED_VIEWPORTS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.by_chat.remove(&evicted);
        }
    }

    pub(super) fn get_cloned_and_touch(&mut self, chat_id: &str) -> Option<SavedViewport> {
        let viewport = self.by_chat.get(chat_id).cloned()?;
        self.recency.retain(|candidate| candidate != chat_id);
        self.recency.push_back(chat_id.to_string());
        Some(viewport)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_chat.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::RowKind;
    use gpui::{ListAlignment, ListState};

    #[test]
    fn selection_scroll_ramps_at_viewport_edges() {
        let bounds = Bounds::new(
            gpui::point(px(10.0), px(20.0)),
            gpui::size(px(300.0), px(200.0)),
        );
        assert_eq!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(120.0))),
            0.0
        );
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(20.0))) < 0.0);
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0))) > 0.0);
        assert!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0)))
                > selection_scroll_step(bounds, gpui::point(px(20.0), px(200.0)))
        );
    }

    // ---- stick-to-bottom spring ----

    #[test]
    fn spring_converges_to_a_fixed_target() {
        let mut spring = StickSpring::new();
        let target = 400.0;
        let mut pos = 0.0;
        let mut frames = 0;
        while pos < target && frames < 600 {
            pos = spring.step(pos, target, 1.0);
            frames += 1;
        }
        assert_eq!(pos, target, "spring must land exactly on the target");
        assert!(
            frames < 300,
            "400px should converge within 5s of frames, took {frames}"
        );
        // Once landed it stays landed (and idles out).
        for _ in 0..120 {
            pos = spring.step(pos, target, 1.0);
            assert_eq!(pos, target);
        }
        assert!(spring.is_idle(), "no residual motion at rest");
    }

    #[test]
    fn spring_never_overshoots_or_oscillates() {
        let mut spring = StickSpring::new();
        let target = 250.0;
        let mut pos = 0.0;
        let mut last = pos;
        for _ in 0..600 {
            pos = spring.step(pos, target, 1.0);
            assert!(pos <= target, "overshoot: {pos} > {target}");
            assert!(
                pos >= last - 1e-3,
                "oscillation: position moved backwards {last} -> {pos}"
            );
            last = pos;
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn spring_feed_forward_tracks_constant_growth() {
        // Target grows 2px/frame (≈120px/s — a typical stream). After warmup
        // the EMA feed-forward must carry the viewport at the same rate with a
        // bounded, stable lag — a glide, not 0,0,0,Npx steps.
        let growth = 2.0;
        let mut spring = StickSpring::new();
        let mut target = 600.0;
        let mut pos = 600.0;
        let mut deltas: Vec<f32> = Vec::new();
        for frame in 0..400 {
            target += growth;
            let next = spring.step(pos, target, 1.0);
            if frame >= 200 {
                deltas.push(next - pos);
            }
            pos = next;
        }
        // Steady state: per-frame movement ≈ growth rate…
        let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
        assert!(
            (mean - growth).abs() < 0.2,
            "steady-state speed {mean} should track growth {growth}"
        );
        // …with no stepping (every frame moves, none jumps).
        for d in &deltas {
            assert!(*d > 0.0, "viewport stalled mid-stream");
            assert!(*d < growth * 3.0, "viewport jumped: {d}px in one frame");
        }
        // The EMA growth estimate itself has locked on.
        assert!((spring.target_vel() - growth).abs() < 0.3);
        // Lag stays bounded by the chase lead.
        assert!(target - pos <= SPRING_CHASE_MAX_LEAD + growth);
    }

    #[test]
    fn spring_feed_forward_resets_when_target_shrinks() {
        let mut spring = StickSpring::new();
        let mut pos = 0.0;
        for i in 1..=50 {
            pos = spring.step(pos, 100.0 + i as f32 * 4.0, 1.0);
        }
        assert!(spring.target_vel() > 1.0);
        // A collapse (target shrinks by more than 1px) drops the estimate.
        spring.step(pos.min(120.0), 120.0, 1.0);
        assert_eq!(spring.target_vel(), 0.0);
    }

    #[test]
    fn spring_catchup_frames_glide_instead_of_teleporting() {
        // A 5-frame hitch advances roughly as far as 5 single steps would —
        // sub-stepped, still clamped at the target.
        let target = 300.0;
        let mut a = StickSpring::new();
        let mut pos_a = 0.0;
        for _ in 0..5 {
            pos_a = a.step(pos_a, target, 1.0);
        }
        let mut b = StickSpring::new();
        let pos_b = b.step(0.0, target, 5.0);
        assert!((pos_a - pos_b).abs() < 1.0, "{pos_a} vs {pos_b}");
        assert!(pos_b <= target);
    }

    #[test]
    fn only_a_stream_at_the_bottom_gets_a_hard_end_anchor() {
        assert!(should_anchor_live_stream(true, 0.0, true));
        assert!(should_anchor_live_stream(true, AT_BOTTOM_PX, true));

        // A user who has moved away from the end keeps control of the
        // viewport, even if the transcript is still streaming.
        assert!(!should_anchor_live_stream(true, AT_BOTTOM_PX + 0.1, true));
        assert!(!should_anchor_live_stream(false, 0.0, true));

        // Ordinary transcript updates retain the existing spring behavior.
        assert!(!should_anchor_live_stream(true, 0.0, false));
    }

    #[test]
    fn own_turn_reservation_is_a_min_height_for_the_turn() {
        let usable = 700.0;
        // A short turn reserves the rest of the usable viewport below it.
        assert_eq!(own_turn_reservation(usable, 100.0), 600.0);
        // Growth consumes the reservation 1:1 — total held height is stable.
        assert_eq!(own_turn_reservation(usable, 450.0), 250.0);
        // At/past the fill line nothing is reserved (bottom spring takes
        // over with no height jump).
        assert_eq!(own_turn_reservation(usable, 700.0), 0.0);
        assert_eq!(own_turn_reservation(usable, 1_200.0), 0.0);
    }

    fn viewport_row(id: &str, entry_id: &str) -> Row {
        Row {
            id: id.into(),
            version: 0,
            turn_start: true,
            kind: RowKind::ErrorChip {
                message: SharedString::default(),
            },
            entry_id: entry_id.into(),
            timestamp: None,
            copy_text: None,
        }
    }

    #[test]
    fn viewport_anchor_tracks_a_stable_row_across_replay() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 1,
                offset_in_item: px(23.0),
            },
        )
        .expect("visible row");

        let replay = vec![
            viewport_row("new", "entry-new"),
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let restored = anchor.resolve(&replay).expect("restored row");
        assert_eq!(restored.item_ix, 2);
        assert_eq!(restored.offset_in_item, px(23.0));
    }

    #[test]
    fn viewport_anchor_has_entry_and_index_fallbacks() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("old-block", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 2,
                offset_in_item: px(31.0),
            },
        )
        .expect("visible row");

        let reshaped = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("inserted", "entry-new"),
            viewport_row("new-block", "entry-c"),
        ];
        let same_entry = anchor.resolve(&reshaped).expect("entry fallback");
        assert_eq!(same_entry.item_ix, 3);
        assert_eq!(same_entry.offset_in_item, px(0.0));

        let entry_removed = vec![viewport_row("a", "entry-a"), viewport_row("b", "entry-b")];
        let clamped = anchor.resolve(&entry_removed).expect("index fallback");
        assert_eq!(clamped.item_ix, 1);
        assert_eq!(clamped.offset_in_item, px(0.0));
    }

    #[test]
    fn optimistic_echo_cannot_consume_a_historical_viewport_before_replay() {
        let history = vec![viewport_row("historical", "historical-entry")];
        let saved = SavedViewport::capture(&history, ListOffset::default(), false, 480.0, None)
            .expect("historical viewport");
        let echo_only = vec![viewport_row("echo", "echo-entry")];

        assert!(
            saved.resolve(&echo_only, false).is_none(),
            "an unrelated echo is not an authoritative index fallback"
        );
        assert_eq!(
            saved
                .resolve(&echo_only, true)
                .expect("populated replay may use an index fallback")
                .offset
                .item_ix,
            0
        );
        assert!(TranscriptReplayState::Empty.authoritative_empty());
        assert!(!TranscriptReplayState::Empty.allows_fallback());
        assert!(!TranscriptReplayState::Pending.allows_fallback());
        assert!(TranscriptReplayState::Populated.allows_fallback());

        let echo_viewport =
            SavedViewport::capture(&echo_only, ListOffset::default(), false, 0.0, None)
                .expect("echo viewport");
        assert!(
            echo_viewport.resolve(&echo_only, false).is_some(),
            "the exact optimistic row is safe before replay"
        );
    }

    #[test]
    fn saved_viewport_preserves_and_releases_an_active_turn_runway() {
        let rows = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 640.0,
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &rows,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
            false,
            0.0,
            Some(&own_turn),
        )
        .expect("active chat viewport");
        let SavedViewport::Anchored {
            own_turn: Some(saved_turn),
            ..
        } = &saved
        else {
            panic!("an active turn must keep its runway with the viewport");
        };
        assert_eq!(saved_turn.runway, 640.0);
        assert!(saved_turn.held);
        assert!(saved_turn.positioned);

        let restored = saved
            .resolve(&rows, false)
            .expect("exact queued echo survives an empty replay");
        let restored_turn = restored.own_turn.expect("valid restored runway");
        assert_eq!(restored_turn.runway, 640.0);
        assert!(!restored_turn.held);
        assert!(!restored_turn.positioned);
        assert!(restored_turn.seen_prompt);

        let list_state = ListState::new(rows.len(), ListAlignment::Top, px(0.0));
        list_state.reset(0);
        list_state.splice(0..0, rows.len());
        list_state.scroll_to(restored.offset);
        assert_eq!(list_state.logical_scroll_top().item_ix, 0);
        assert_eq!(list_state.logical_scroll_top().offset_in_item, px(0.0));

        assert!(
            SavedViewport::capture(&[], ListOffset::default(), false, 0.0, Some(&own_turn))
                .is_none(),
            "an empty rapid-switch replay must not overwrite the older snapshot"
        );
    }

    #[test]
    fn own_turn_waits_for_its_first_echo_then_retires_if_it_disappears() {
        let mut turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt: false,
        };

        assert!(turn.observe_prompt(false), "fresh send waits one state gap");
        assert!(turn.observe_prompt(true), "echo activates the runway");
        assert!(turn.seen_prompt);
        assert!(
            !turn.observe_prompt(false),
            "failed echo retires the activated runway"
        );
    }

    #[test]
    fn restored_viewport_discards_a_failed_optimistic_turn() {
        let outgoing = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 640.0,
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &outgoing,
            ListOffset::default(),
            false,
            420.0,
            Some(&own_turn),
        )
        .expect("outgoing viewport");

        // The failed echo vanished while A was hidden. The ordinary viewport
        // still restores by index, but no stale runway may intercept jump.
        let replay = vec![viewport_row("older", "older")];
        let restored = saved.resolve(&replay, true).expect("index fallback");
        assert!(restored.own_turn.is_none());
        assert_eq!(restored.offset.item_ix, 0);
        assert_eq!(restored.distance_from_bottom, 420.0);
    }

    #[test]
    fn pinned_viewports_follow_tail_and_the_cache_is_bounded() {
        let rows = vec![viewport_row("row", "entry")];
        let pinned = SavedViewport::capture(&rows, ListOffset::default(), true, 999.0, None)
            .expect("pinned viewport");
        assert!(matches!(pinned, SavedViewport::FollowTail));

        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS + 8 {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }
        assert_eq!(cache.len(), MAX_SAVED_VIEWPORTS);
        assert!(cache.get_cloned_and_touch("chat-0").is_none());
        assert!(
            cache
                .get_cloned_and_touch(&format!("chat-{}", MAX_SAVED_VIEWPORTS + 7))
                .is_some()
        );
    }

    #[test]
    fn reopening_the_oldest_cached_chat_protects_it_from_the_next_eviction() {
        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }

        assert!(cache.get_cloned_and_touch("chat-0").is_some());
        cache.insert("outgoing-new".into(), SavedViewport::FollowTail);

        assert!(cache.by_chat.contains_key("chat-0"));
        assert!(!cache.by_chat.contains_key("chat-1"));
        assert!(cache.by_chat.contains_key("outgoing-new"));
    }

    #[test]
    fn viewport_finalization_waits_for_current_generation_and_stable_layout() {
        let token = ViewportFinalizeToken {
            generation: 7,
            layout_revision: 11,
        };
        assert!(token.still_current(7));
        assert!(!token.still_current(8));
        assert!(token.layout_settled(11));
        assert!(!token.layout_settled(12));
    }

    #[test]
    fn flavour_words_rotate_every_seven_seconds() {
        let seed = flavour_seed("chat-1");
        assert_eq!(flavour_word(seed, 0), flavour_word(seed, 6));
        assert_ne!(flavour_word(seed, 0), flavour_word(seed, 7));
        // Deterministic per chat; different chats usually differ in phase.
        assert_eq!(flavour_word(seed, 3), flavour_word(seed, 3));
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(92), "1m 32s");
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn sending_bridge_holds_until_the_turn_outdates_the_send() {
        let send = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .unwrap()
            .to_utc();
        let before = send - chrono::Duration::seconds(90);
        let after = send + chrono::Duration::seconds(2);
        // In flight, row still on the previous turn (or no row yet).
        assert!(sending_bridge(Some(send), Some(before)));
        assert!(sending_bridge(Some(send), None));
        // The turn started after the send fired — timer takes over.
        assert!(!sending_bridge(Some(send), Some(after)));
        // No send in flight: never a bridge, whatever the row says.
        assert!(!sending_bridge(None, Some(before)));
        assert!(!sending_bridge(None, None));
    }
}
