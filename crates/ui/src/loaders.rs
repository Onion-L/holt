//! Loaders: the holt pulse loader, the three-dot pulse spinners, and the boot
//! splash content. All motion routes through `crate::motion` pure helpers, so
//! the math is unit-tested and these elements are testable-by-compile.
//!
//! Rendering pattern: each cell is its own repeating element sharing one
//! period; per-cell offsets come from [`motion::staggered_phase`], so all
//! cells stay phase-locked (they start on the same frame) without a shared
//! clock. Cells animate inside fixed-size slots — opacity and inner size
//! are paint-local and never move surrounding layout. Reduced motion snaps every
//! cell to its rest state automatically (gpui `reduce_motion`).

use gpui::{
    AnyElement, App, Div, EntityId, IntoElement, ParentElement, PathBuilder, SharedString, Styled,
    canvas, div, point, px,
};

use crate::motion::{self, DOT_PULSE, HOLT_PULSE, PULSE_STAGGER, SPLASH_OUT};
use crate::theme::{GlyphPalette, Theme};

// Shared with the terminal viewport (`holt_proto::motion`) so both animate the
// same loaders from the same numbers.
pub use holt_proto::motion::{HOLT_CELLS, MARK_CELLS, MARK_SPREAD, MATRIX_SIDE, mark_cell_stagger};

/// The animated holt mark (holt-loader.tsx `HoltLoader`): the full logo
/// pixel grid with a light wave sweeping tail→head. Each cell rests dim
/// (opacity 0.08, scale 0.9) and flares to full as the crest passes; per-cell
/// stagger follows the flight axis. `height_px` sets the mark's height (width
/// follows the 820:940 canvas).
pub fn holt_mark_loader(
    _id: &'static str,
    theme: &Theme,
    height_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let scale = height_px / 940.0;
    let cell = 100.0 * scale;
    let delta = motion::pulse_delta(&HOLT_PULSE, view, cx);
    div()
        .relative()
        .w(px(820.0 * scale))
        .h(px(height_px))
        .children(MARK_CELLS.iter().map(move |&(x, y)| {
            let stagger = mark_cell_stagger(x, y);
            // Fixed slot; the animated cell breathes inside it (paint-local).
            div()
                .absolute()
                .left(px(x * scale))
                .top(px(y * scale))
                .size(px(cell))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    // Negative CSS delay ⇒ the cell starts mid-cycle:
                    // the stagger ADDS phase (holt-loader.tsx delayFor).
                    let phase = (delta + stagger).rem_euclid(1.0);
                    div()
                        .rounded(px(16.0 * scale))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(cell * motion::pulse_scale(phase)))
                })
        }))
}

/// The holt wave loader: a row of cells pulsing opacity 0.08→1 / scale 0.9→1
/// over 2.4s with a 0.15s stagger per cell.
///
/// `id` scopes the per-cell animation state — give each loader instance a
/// distinct id.
pub fn holt_loader(
    _id: &'static str,
    theme: &Theme,
    cell_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let slot = cell_px;
    let delta = motion::pulse_delta(&HOLT_PULSE, view, cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(slot / 2.0))
        .children((0..HOLT_CELLS).map(move |i| {
            // Fixed slot; the animated cell breathes inside it.
            div()
                .size(px(slot))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    let phase = motion::staggered_phase(delta, i, PULSE_STAGGER);
                    div()
                        .rounded(px(slot / 4.0))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(slot * motion::pulse_scale(phase)))
                })
        }))
}

pub use holt_proto::motion::GSPIN_DIM;

/// The working indicator (the old 3×3 gradient matrix, flattened): three dots
/// in a row riding one pulse wave left→right, one accent step each — the
/// theme's [`GlyphPalette`] (light→mid→deep), so the indicator follows the
/// user's accent instead of the old hardcoded sunrise brand tints. Dots rest
/// dim ([`GSPIN_DIM`]) inside fixed-size slots, so the row never shifts
/// layout. `cell_px` keeps the old matrix-cell meaning; dots run 1.75× so the
/// slimmed row keeps a similar visual mass at the call sites.
pub fn gradient_spinner(
    _id: &'static str,
    theme: &Theme,
    cell_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let delta = motion::pulse_delta(&DOT_PULSE, view, cx);
    three_dot_pulse(delta, cell_px * 1.75, theme.glyph.rows())
}

/// Three-pulse activity dots sized for compact status slots. Its color is an
/// explicit accent-preset role supplied by the caller, tinting the dots as the
/// wave sweeps left→right.
pub fn mini_glyph_spinner(
    key: impl Into<SharedString>,
    cell_px: f32,
    palette: GlyphPalette,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    mini_spinner_tinted(key, cell_px, palette.rows(), view, cx)
}

/// Grayscale variant for surfaces where an accent would pull focus (the
/// sidebar connection line): same dots, wave, and timing, color left to the
/// caller.
pub fn mini_mono_spinner(
    key: impl Into<SharedString>,
    cell_px: f32,
    tint: gpui::Hsla,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    mini_spinner_tinted(key, cell_px, [tint; 3], view, cx)
}

fn mini_spinner_tinted(
    key: impl Into<SharedString>,
    cell_px: f32,
    row_tints: [gpui::Hsla; 3],
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let _key = key.into();
    let delta = motion::pulse_delta(&DOT_PULSE, view, cx);
    // Compact slots: dots 1.5× the old cell unit so the wave still reads at
    // 2px-scale call sites.
    three_dot_pulse(delta, cell_px * 1.5, row_tints)
}

/// The shared three-dot pulse paint: round dots in a row, each breathing
/// opacity [`GSPIN_DIM`]→1 and scale 0.9→1 inside a FIXED-size slot, staggered
/// a third of the period so the crest hands off dot to dot. Reduce motion pins
/// [`motion::pulse_delta`] at phase 0 — three quiet dim dots, nothing
/// scheduled.
fn three_dot_pulse(delta: f32, dot_px: f32, tints: [gpui::Hsla; 3]) -> Div {
    /// Dot stagger as a fraction of the period: the wave crosses dot→dot in
    /// 400ms of the 1.2s [`DOT_PULSE`].
    const DOT_STAGGER: f32 = 1.0 / 3.0;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(dot_px / 2.0))
        .children((0..3).map(move |i| {
            // Fixed slot; the dot breathes inside it (paint-local).
            div()
                .size(px(dot_px))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    let phase = motion::staggered_phase(delta, i, DOT_STAGGER);
                    div()
                        .rounded(px(dot_px / 2.0))
                        .bg(tints[i])
                        .opacity(motion::lerp(GSPIN_DIM, 1.0, motion::pulse_wave(phase)))
                        .size(px(dot_px * motion::pulse_scale(phase)))
                })
        }))
}

/// Stroke width of [`upload_progress_ring`].
const RING_STROKE: f32 = 2.5;
/// Polyline segments for a full circle — plenty for a ≤40px ring.
const RING_SEGMENTS: f32 = 64.0;

/// Radial upload-progress ring with the percent centered — overlaid on a
/// sending echo's attachment thumbnail while its bytes cross the relay
/// (2026-08-18 "Sending… forever" report; the thumbnail is where the wait
/// visibly belongs). A faint full track plus a bright arc growing clockwise
/// from 12 o'clock; gpui paths have no arc primitive, so both are stroked
/// polylines. Fixed white-on-wash palette: the caller dims the image behind
/// it, which reads in both themes.
pub fn upload_progress_ring(percent: u8, diameter: f32) -> AnyElement {
    let frac = f32::from(percent.min(100)) / 100.0;
    let ring = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let radius = diameter / 2.0 - RING_STROKE;
            let mut paint_arc = |sweep: f32, color: gpui::Hsla| {
                if sweep <= 0.0 {
                    return;
                }
                let steps = ((RING_SEGMENTS * sweep).ceil() as usize).max(2);
                let at = |i: usize| {
                    // Clockwise from 12 o'clock.
                    let theta = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * sweep * (i as f32 / steps as f32);
                    point(
                        center.x + px(radius * theta.cos()),
                        center.y + px(radius * theta.sin()),
                    )
                };
                let mut builder = PathBuilder::stroke(px(RING_STROKE));
                builder.move_to(at(0));
                for i in 1..=steps {
                    builder.line_to(at(i));
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            };
            paint_arc(1.0, gpui::hsla(0.0, 0.0, 1.0, 0.22));
            paint_arc(frac, gpui::hsla(0.0, 0.0, 1.0, 0.95));
        },
    )
    .absolute()
    .inset_0();
    div()
        .relative()
        .size(px(diameter))
        .flex()
        .items_center()
        .justify_center()
        .child(ring)
        .child(
            div()
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui::hsla(0.0, 0.0, 1.0, 0.95))
                .child(SharedString::from(format!("{percent}%"))),
        )
        .into_any_element()
}

/// Full-window boot splash: the app's dot loader (the same [`gradient_spinner`]
/// the session list and the reconnecting line pulse — user request, replacing
/// the hero ascii) over the app background with a quiet status line. While
/// `fading` it plays `splash-out` (150ms hold, then 0.5s fade + 6px lift); the
/// shell removes it once [`SPLASH_OUT`] has run its course.
pub fn splash_overlay(theme: &Theme, fading: bool, view: EntityId, cx: &mut App) -> AnyElement {
    let content = div()
        .absolute()
        .inset_0()
        // Frosted glass, not the opaque page tone (user request): the boot
        // overlay reads like the rest of the chrome — the frost tint over
        // the blurred window background (opaque platforms get the surface
        // tone, since `glass()` collapses to it there).
        .bg(theme.glass())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        // Cell 2.5 — the size every other surface runs this spinner at (the
        // "Sending…" strip, the transcript working trailer).
        .child(gradient_spinner(
            "boot-splash-spinner",
            theme,
            2.5,
            view,
            cx,
        ))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from("Setting up Holt environment")),
        );
    if fading {
        motion::splash_out("boot-splash-out", content).into_any_element()
    } else {
        content.into_any_element()
    }
}

// Compile-time proof the specs referenced here stay wired to the catalog.
const _: () = {
    assert!(SPLASH_OUT.delay_ms == 150);
    assert!(HOLT_PULSE.duration_ms == 2400);
    assert!(DOT_PULSE.duration_ms == 1200);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_stagger_follows_flight_axis() {
        // Tail tip (720, 0) leads: near-maximal stagger (starts deepest into
        // the cycle); head (0, 840) trails with stagger 0.
        let tail = mark_cell_stagger(720.0, 0.0);
        let head = mark_cell_stagger(0.0, 840.0);
        assert!(tail > head, "tail {tail} should lead head {head}");
        assert!((head - 0.0).abs() < 1e-6, "head stagger ≈ 0, got {head}");
        assert!(tail <= MARK_SPREAD + 1e-6, "stagger capped at SPREAD");
        // Every logo cell stays inside [0, SPREAD].
        for &(x, y) in &MARK_CELLS {
            let s = mark_cell_stagger(x, y);
            assert!(
                (0.0..=MARK_SPREAD + 1e-6).contains(&s),
                "cell ({x},{y}) stagger {s}"
            );
        }
    }
}
