//! The compact↔expanded flip morph: ONE committed flip eases the pill height
//! between layouts while the control cluster stays pinned to the stationary
//! bottom edge and only the text glides.

use crate::motion;
use crate::theme::Theme;

/// Compact↔expanded flip morph (round 9): the flip used to snap between the
/// two pill layouts. The original has no height transition (its shell carries
/// only `transition-colors`), so this is a native nicety: ONE committed flip
/// starts exactly one 180ms ease-out morph ([`motion::COLLAPSE`], the same
/// manual-drive pattern as shell.rs `WidthTween` — never `with_animation`,
/// whose element-id keying replays tweens on remount, round-6 §1–3).
///
/// The morph animates the pill's COMMITTED height: the flip commits its final
/// layout immediately (the input entity never remounts — the caret survives,
/// exactly as before) while the pill clips toward the live target. The pill's
/// bottom edge is stationary on screen, so the controls stay pinned to it
/// (constant screen-y; see the anchoring helpers below) and only the text
/// glides with the sweeping top edge. [`composer_flip`]'s hysteresis already
/// guarantees no oscillation at the boundary, and [`flip_morph_step`] never
/// restarts a morph while the committed mode holds. Reduced motion snaps: no
/// morph is ever created.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlipMorph {
    /// Rendered height when the flip committed — the animation's start point.
    pub from: f32,
    /// Commit time in ms on the caller's monotonic clock.
    pub start_ms: f32,
}

impl FlipMorph {
    /// Raw timeline position 0..1 over [`motion::COLLAPSE`]'s 180ms.
    fn raw(&self, now_ms: f32) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32() * 1000.0;
        ((now_ms - self.start_ms) / total).clamp(0.0, 1.0)
    }

    /// Eased progress 0..1 (ease-out) — also drives the actions fade.
    pub fn progress(&self, now_ms: f32) -> f32 {
        motion::COLLAPSE.progress(self.raw(now_ms))
    }

    pub fn done(&self, now_ms: f32) -> bool {
        self.raw(now_ms) >= 1.0
    }

    /// Committed-height evaluation: eased lerp from the flip-time height to
    /// the LIVE target (auto-grow may move the target mid-morph — the morph
    /// tracks it instead of finishing on a stale height).
    pub fn height(&self, target: f32, now_ms: f32) -> f32 {
        motion::lerp(self.from, target, self.progress(now_ms))
    }
}

// -- morph anchoring (round-9 follow-up) ------------------------------------
// The pill sits at the BOTTOM of the shell column: growing it moves its TOP
// edge; the bottom edge is stationary on screen. The first morph cut anchored
// the pill's inner content to the top, so the actions/cluster (laid out at
// the inner bottom) rode the animating height up and down. The controls are
// therefore pinned to the stationary bottom edge (absolute bottom row when
// expanded, a bottom-justified row when compact) and only the TEXT glides
// with the sweeping top edge. The helpers below are the pure math.

/// Send/attach center sits 27px above the pill's outer bottom in expanded
/// mode (`pb-2.5` 10 + half the 32px content zone + 1px hairline) but 24.5px
/// in compact (centered in the 47px row) — an inherent 2.5px delta between
/// the two SOURCE geometries. The morph glides it instead of snapping.
pub const CLUSTER_Y_DELTA: f32 = 2.5;

/// The cluster's INTERNAL geometry is mode-independent. Reasoning/service
/// tier and attachment form one utility group; Send is a distinct primary
/// action. Both layouts reuse these distances so the flip cannot create a
/// horizontal compression pulse.
/// Only the wrapper's right inset differs: `pr-2` (8) compact vs `px-3` (12)
/// expanded — a whole-cluster 4px shift that glides with the morph.
pub const CLUSTER_X_DELTA: f32 = 4.0;
/// Optical join between the picker group and the paperclip. This is tighter
/// than the structural spacing ladder because the narrow paperclip glyph
/// otherwise looks farther away than its hit target actually is.
pub const ACTION_UTILITY_GAP: f32 = 2.0;
/// Structural separation between utility actions and the primary Send action.
pub const ACTION_PRIMARY_GAP: f32 = Theme::SPACE_SM;

/// The right inset for the in-flight morph: eases from the OLD mode's resting
/// inset to the committed mode's (compact 8 ↔ expanded 12) — pairwise button
/// distances stay constant; the cluster glides as one.
pub fn morph_cluster_inset(expanded: bool, progress: f32) -> f32 {
    let (from, to) = if expanded {
        (8.0, 8.0 + CLUSTER_X_DELTA)
    } else {
        (8.0 + CLUSTER_X_DELTA, 8.0)
    };
    motion::lerp(from, to, progress)
}

/// Expanded text top padding across the morph: starts at the compact resting
/// inset (12 ≈ `py-3`) and eases to `pt-4` (16) — the first line glides with
/// the rising top edge instead of jumping at the commit.
pub fn morph_text_pad(progress: f32) -> f32 {
    motion::lerp(12.0, 16.0, progress)
}

/// Collapse-morph text glide: the committed compact row is bottom-anchored
/// (text resting top = 36px above the pill's outer bottom: 49 − 1 hairline −
/// 12 centering inset), while at the commit instant the text sat 17px below
/// the expanded pill's top (1 hairline + 16 `pt-4`) — i.e. `from − 17` above
/// the bottom. The decaying relative offset walks it down smoothly.
pub fn collapse_text_glide(from: f32, progress: f32) -> f32 {
    (from - 53.0).max(0.0) * (1.0 - progress)
}

/// The decaying [`CLUSTER_Y_DELTA`] offset for the in-flight morph.
/// The whole control cluster — chips AND attach/send — rides the stationary
/// bottom anchor at FULL alpha throughout (round-9 follow-up: any fade on the
/// picker chips read as flicker; their screen position is near-stationary
/// across the flip, so nothing needs to be hidden).
pub fn morph_cluster_dy(progress: f32) -> f32 {
    CLUSTER_Y_DELTA * (1.0 - progress)
}

/// Session/route changes SNAP the composer (same rule as the header inset
/// tween, round 6: route swaps remount in the original — zero motion). The
/// nav-driven flip doesn't commit on the first render after a switch (the
/// draft swap has to be laid out and re-measured first), so a plain reset at
/// the nav instant leaks: `last_rendered_height` is repopulated before the
/// flip lands and the session change morphs 49↔124. Instead, every flip
/// committed within this wall-clock window of a navigation snaps. User-driven
/// flips need typing and can't land this fast after a switch.
pub const ROUTE_SNAP_MS: u64 = 250;

/// Advance the flip morph across one render pass. While the committed mode
/// holds, the morph is kept (a finished one clears) — same-mode renders can
/// NEVER restart the animation. A committed mode change starts one morph from
/// the last rendered height, which mid-flight is the CURRENT animated height,
/// so a reverse flip hands off seamlessly instead of popping to an endpoint.
/// Reduced motion (or a first paint with no measured height yet) snaps, and
/// `route_snap` (a session/route change within [`ROUTE_SNAP_MS`]) both blocks
/// arming AND kills anything in flight — navigation never animates the pill.
pub fn flip_morph_step(
    morph: Option<FlipMorph>,
    mode_changed: bool,
    last_height: f32,
    now_ms: f32,
    reduced_motion: bool,
    route_snap: bool,
) -> Option<FlipMorph> {
    if route_snap {
        return None;
    }
    if !mode_changed {
        return morph.filter(|m| !m.done(now_ms));
    }
    if reduced_motion || last_height <= 0.0 {
        return None;
    }
    Some(FlipMorph {
        from: last_height,
        start_ms: now_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame short of the full morph timeline (never rounds up to done).
    const ALMOST: f32 = 179.0;

    #[test]
    fn flip_morph_starts_once_per_committed_flip() {
        // No committed flip → no morph.
        assert_eq!(flip_morph_step(None, false, 49.0, 0.0, false, false), None);
        // A committed flip starts one, from the last rendered height…
        let m = flip_morph_step(None, true, 49.0, 100.0, false, false).unwrap();
        assert_eq!(m.from, 49.0);
        assert_eq!(m.start_ms, 100.0);
        // …and same-mode renders keep it UNCHANGED (no restart at the
        // boundary, whatever the heights are doing).
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 150.0, false, false),
            Some(m)
        );
        // A finished morph clears on the next same-mode render.
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 100.0 + ALMOST, false, false),
            Some(m)
        );
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 300.0, false, false),
            None
        );
    }

    #[test]
    fn flip_morph_height_ramps_monotonically_to_target() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Starts exactly at the committed height…
        let mut prev = m.height(124.0, 0.0);
        assert_eq!(prev, 49.0);
        // …ramps without ever moving backwards…
        for step in 1..=18 {
            let h = m.height(124.0, step as f32 * 10.0);
            assert!(h >= prev, "height regressed at {step}: {h} < {prev}");
            prev = h;
        }
        // …and lands exactly on the target when done (and stays there).
        assert_eq!(m.height(124.0, 180.0), 124.0);
        assert!(m.done(180.0));
        assert_eq!(m.height(124.0, 500.0), 124.0);
        // Collapse runs the same ramp downward.
        assert!(m.height(124.0, 90.0) > 49.0);
        let down = FlipMorph {
            from: 124.0,
            start_ms: 0.0,
        };
        assert!(down.height(49.0, 90.0) < 124.0);
        assert!(down.height(49.0, 90.0) > 49.0);
    }

    #[test]
    fn flip_morph_reverse_hands_off_from_current_height() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        let mid = m.height(124.0, 90.0);
        assert!(mid > 49.0 && mid < 124.0);
        // A reverse flip mid-flight commits a new morph FROM the animated
        // height — continuous at the handoff, no pop to an endpoint.
        let rev = flip_morph_step(Some(m), true, mid, 90.0, false, false).unwrap();
        assert_eq!(rev.from, mid);
        assert_eq!(rev.height(49.0, 90.0), mid);
    }

    #[test]
    fn flip_morph_snaps_for_reduced_motion_and_first_paint() {
        // Reduced motion never creates a morph (the flip just snaps)…
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, true, false), None);
        // …and neither does a flip before anything was ever rendered.
        assert_eq!(flip_morph_step(None, true, 0.0, 0.0, false, false), None);
    }

    #[test]
    fn route_change_never_arms_the_morph() {
        // A flip committed inside the route-snap window must NOT animate —
        // switching sessions (chat↔chat or chat↔new-session) snaps the
        // composer straight to the target mode, like the header (round 6).
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, false, true), None);
        // The route change also kills anything already in flight…
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 50.0, false, true),
            None
        );
        assert_eq!(
            flip_morph_step(Some(m), true, 80.0, 50.0, false, true),
            None
        );
        // …while outside the window the same flip animates as usual.
        let armed = flip_morph_step(None, true, 49.0, 300.0, false, false).unwrap();
        assert_eq!(armed.from, 49.0);
    }

    #[test]
    fn morph_anchoring_holds_controls_and_glides_text() {
        // Steady state (progress 1): no offsets, everything at rest.
        assert_eq!(morph_cluster_dy(1.0), 0.0);
        assert_eq!(morph_text_pad(1.0), 16.0);
        assert_eq!(collapse_text_glide(124.0, 1.0), 0.0);
        // At the commit instant the pieces start from the OLD mode's resting
        // geometry: text pad at the compact 12px inset, cluster displaced by
        // exactly the 2.5px centering delta.
        assert_eq!(morph_text_pad(0.0), 12.0);
        assert_eq!(morph_cluster_dy(0.0), CLUSTER_Y_DELTA);
        // Collapse glide: starts where the expanded text sat (17px below the
        // committed pill top → `from − 53` above the compact resting spot)…
        assert_eq!(collapse_text_glide(124.0, 0.0), 71.0);
        // …decays monotonically to zero…
        let mut prev = collapse_text_glide(124.0, 0.0);
        for step in 1..=10 {
            let g = collapse_text_glide(124.0, step as f32 / 10.0);
            assert!(g <= prev, "glide regressed at {step}");
            prev = g;
        }
        // …and can't go negative on shallow mid-flight reversals.
        assert_eq!(collapse_text_glide(50.0, 0.0), 0.0);
    }

    #[test]
    fn cluster_inset_glides_between_the_source_endpoints() {
        assert_eq!(ACTION_UTILITY_GAP, 2.0);
        assert_eq!(ACTION_PRIMARY_GAP, Theme::SPACE_SM);
        assert!(ACTION_UTILITY_GAP < ACTION_PRIMARY_GAP);
        // The morph starts from the OLD mode's resting inset (no sideways
        // step at the commit) and eases to the committed mode's…
        assert_eq!(morph_cluster_inset(true, 0.0), 8.0); // expand: from compact pr-2
        assert_eq!(morph_cluster_inset(true, 1.0), 12.0); // …to expanded px-3
        assert_eq!(morph_cluster_inset(false, 0.0), 12.0); // collapse: from px-3
        assert_eq!(morph_cluster_inset(false, 1.0), 8.0); // …to pr-2
        // …monotonically, bounded by the 4px source delta.
        let mut prev = morph_cluster_inset(true, 0.0);
        for step in 1..=10 {
            let v = morph_cluster_inset(true, step as f32 / 10.0);
            assert!(v >= prev && v <= 8.0 + CLUSTER_X_DELTA);
            prev = v;
        }
        // Internal group spacing is shared between modes — only this wrapper
        // inset may differ across the flip.
    }

    #[test]
    fn flip_morph_tracks_live_target_and_drives_fade() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Auto-grow can move the target mid-morph: evaluation tracks the
        // live value instead of finishing on a stale height.
        assert!(m.height(159.0, 90.0) > m.height(124.0, 90.0));
        // The eased progress is the actions-row fade: 0 at commit, 1 at rest.
        assert_eq!(m.progress(0.0), 0.0);
        assert_eq!(m.progress(180.0), 1.0);
        let mid = m.progress(90.0);
        assert!(mid > 0.0 && mid < 1.0);
    }
}
