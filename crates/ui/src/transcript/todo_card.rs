//! The task-list card (issue #47): one checklist card per Turn, rewritten
//! in place by each `todo` call. Passive rendering — no interaction — but
//! the card is alive: a status flip pops its glyph once, and the header's
//! progress strip glides to the new fraction.
//!
//! The animations are remount-proof by construction: they key on a
//! transition MEMORY (the last rendered statuses and fraction), not on
//! element identity, so a virtualized-list remount — or a store prune
//! followed by a reseed — renders the settled state statically instead of
//! replaying. Reduced motion snaps every tween through gpui's own flag.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, SharedString, div, prelude::*, px};

use holt_doc::{TodoCardItem, TodoItemStatus};

use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

use super::Transcript;

/// The status-flip pop: quick fade + 2px rise.
const FLIP: motion::MotionSpec = motion::MotionSpec::new(180, motion::EASE);
/// The progress strip's glide to a new fraction.
const GLIDE: motion::MotionSpec = motion::MotionSpec::new(320, motion::EASE_OUT_EXPO);
/// How long a flip stays animated after it armed; past the window the slot
/// renders statically (a stale armed flip — a paused repaint — never pops
/// late).
const FLIP_WINDOW: Duration = Duration::from_millis(260);
/// A card unrendered this long drops its motion memory; the next render
/// reseeds from the current state, so nothing replays.
const STORE_KEEP: Duration = Duration::from_secs(30);

/// What one card showed last render: item statuses (positional — the list
/// rewrites whole) with a flip clock per moved slot, and the progress
/// strip's displayed fraction mid-glide.
struct RowMotion {
    statuses: Vec<TodoItemStatus>,
    flips: HashMap<usize, Instant>,
    from: f32,
    to: f32,
    gliding_since: Option<Instant>,
    seen: Instant,
}

/// The per-card transition memory, thread-local like the hover fades —
/// element builders reach it without threading `cx`.
struct TodoMotion {
    rows: HashMap<String, RowMotion>,
    pruned: Instant,
}

impl Default for TodoMotion {
    fn default() -> Self {
        Self {
            rows: HashMap::new(),
            pruned: Instant::now(),
        }
    }
}

impl TodoMotion {
    fn rows(&mut self) -> &mut HashMap<String, RowMotion> {
        // Time-based GC, not per frame: a stale key is one HashMap entry,
        // and a prune pass costs one sweep every STORE_KEEP at most.
        if self.pruned.elapsed() >= STORE_KEEP {
            self.rows.retain(|_, row| row.seen.elapsed() < STORE_KEEP);
            self.pruned = Instant::now();
        }
        &mut self.rows
    }
}

thread_local! {
    static TODO_MOTION: std::cell::RefCell<TodoMotion> = Default::default();
}

/// What the card renders this frame: which slots are inside their flip
/// window, the progress fraction to display, and whether any tween still
/// needs frames. Detects transitions against the store and records them —
/// one call per card render.
fn motion_frame(
    row_id: &str,
    statuses: &[TodoItemStatus],
    fraction: f32,
) -> (Vec<bool>, f32, bool) {
    TODO_MOTION.with(|motion| {
        let now = Instant::now();
        let mut motion = motion.borrow_mut();
        let row = motion
            .rows()
            .entry(row_id.to_string())
            .or_insert_with(|| RowMotion {
                statuses: statuses.to_vec(),
                flips: HashMap::new(),
                from: fraction,
                to: fraction,
                gliding_since: None,
                seen: now,
            });
        row.seen = now;

        // Flips: a status that changed since the last render pops its glyph
        // once, within the window. Slots beyond the old length (added items)
        // are entrances, not flips — the card's own fade covers them.
        let mut flips = Vec::with_capacity(statuses.len());
        for (ix, status) in statuses.iter().enumerate() {
            let flipping = match row.statuses.get(ix) {
                Some(previous) if previous != status => {
                    row.flips.insert(ix, now);
                    true
                }
                Some(_) => row
                    .flips
                    .get(&ix)
                    .is_some_and(|armed| now.duration_since(*armed) < FLIP_WINDOW),
                None => false,
            };
            flips.push(flipping);
        }
        row.statuses = statuses.to_vec();

        // The strip glides from wherever it visibly stood to the new
        // fraction; an unchanged fraction renders static.
        let displayed = match row.gliding_since {
            Some(since) => {
                let t = GLIDE.progress(
                    (now.duration_since(since).as_secs_f32() / GLIDE.total().as_secs_f32())
                        .clamp(0.0, 1.0),
                );
                if t >= 1.0 {
                    row.gliding_since = None;
                    row.from = row.to;
                    row.to
                } else {
                    motion::lerp(row.from, row.to, t)
                }
            }
            None => row.to,
        };
        if (row.to - fraction).abs() > f32::EPSILON {
            row.from = displayed;
            row.to = fraction;
            row.gliding_since = Some(now);
        }

        let animating = flips.iter().any(|flipping| *flipping) || row.gliding_since.is_some();
        (flips, displayed, animating)
    })
}

#[cfg(test)]
fn reset_motion() {
    TODO_MOTION.with(|motion| motion.borrow_mut().rows.clear());
}

/// The status glyph's 16px box: a hairline ring for pending, an accent
/// ring with a breathing dot for in-progress (the shared pulse clock —
/// paint-local, fixed footprint), a filled check for done, a dash for
/// cancelled.
fn status_glyph(
    status: TodoItemStatus,
    live: bool,
    theme: &Theme,
    view: Option<gpui::EntityId>,
    cx: &mut App,
) -> gpui::Div {
    let frame = div()
        .flex_none()
        .size(px(16.0))
        .flex()
        .items_center()
        .justify_center();
    match status {
        TodoItemStatus::Pending | TodoItemStatus::Unknown => frame.child(
            div()
                .size(px(13.0))
                .rounded_full()
                .border_1()
                .border_color(theme.hairline(0.28)),
        ),
        TodoItemStatus::InProgress => {
            // The breathing dot rides the shared pulse clock while the Turn
            // is live; a settled Turn (or a clock-less render) holds it lit.
            let phase = match (live, view) {
                (true, Some(view)) => motion::pulse_delta(&motion::DOT_PULSE, view, cx),
                _ => 0.0,
            };
            let dot_opacity = match (live, view) {
                (true, Some(_)) => 0.35 + 0.65 * motion::pulse_wave(phase),
                _ => 1.0,
            };
            frame.child(
                div()
                    .size(px(13.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.accent.opacity(0.45))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(6.0))
                            .rounded_full()
                            .bg(theme.accent)
                            .opacity(dot_opacity),
                    ),
            )
        }
        TodoItemStatus::Done => frame.child(
            div()
                .size(px(14.0))
                .rounded_full()
                .bg(theme.success)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(crate::icons::CHECK)
                        .size(px(9.0))
                        .text_color(theme.bg),
                ),
        ),
        TodoItemStatus::Cancelled => frame.child(
            div()
                .w(px(8.0))
                .h(px(1.5))
                .rounded_full()
                .bg(theme.text_faint),
        ),
    }
}

impl Transcript {
    /// The task-list card: header (icon tile, "Todo", the done count) over
    /// the gliding progress strip, then one row per item. Passive — the
    /// card is the Turn narrating its own progress, not a control.
    pub(super) fn render_todo_card(
        &mut self,
        row_id: &SharedString,
        items: &[TodoCardItem],
        live: bool,
        theme: &Theme,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let done = items
            .iter()
            .filter(|item| item.status == TodoItemStatus::Done)
            .count();
        let fraction = if items.is_empty() {
            0.0
        } else {
            done as f32 / items.len() as f32
        };
        let statuses: Vec<TodoItemStatus> = items.iter().map(|item| item.status).collect();
        let fresh = TODO_MOTION.with(|motion| !motion.borrow().rows.contains_key(row_id.as_ref()));
        let (flips, displayed, animating) = motion_frame(row_id, &statuses, fraction);
        if animating {
            // Keep frames coming while a tween is mid-flight — the veil's
            // pattern: one self-limiting callback per frame.
            let id = cx.entity_id();
            window.on_next_frame(move |_, cx| cx.notify(id));
        }
        card_element(
            row_id,
            items,
            live,
            &flips,
            displayed,
            fresh,
            theme,
            Some(cx.entity_id()),
            cx,
        )
    }
}

/// The card element, pure over its inputs (the motion frame is computed by
/// the caller): header, gliding strip, one row per item.
#[allow(clippy::too_many_arguments)]
fn card_element(
    row_id: &str,
    items: &[TodoCardItem],
    live: bool,
    flips: &[bool],
    displayed: f32,
    fresh: bool,
    theme: &Theme,
    view: Option<gpui::EntityId>,
    cx: &mut App,
) -> AnyElement {
    let done = items
        .iter()
        .filter(|item| item.status == TodoItemStatus::Done)
        .count();
    let tile = div()
        .flex_none()
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.hairline(0.10))
        .bg(theme.ink(0.04))
        .child(
            crate::icons::icon(crate::icons::CHECKLIST)
                .size(px(14.0))
                .text_color(theme.text_muted),
        );
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .pt(px(10.0))
        .pb(px(8.0))
        .child(tile)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_size(crate::typography::ui_rems(13.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child("Todo"),
        )
        .child(
            div()
                .flex_none()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(format!("{done}/{}", items.len()))),
        );
    // The progress strip: a 2px track under the header, the accent fill
    // gliding to the new fraction on each write.
    let strip = div()
        .mx(px(12.0))
        .h(px(2.0))
        .rounded_full()
        .bg(theme.ink(0.06))
        .child(
            div()
                .h_full()
                .w(gpui::relative(displayed))
                .rounded_full()
                .bg(theme.accent),
        );

    let rows = items.iter().enumerate().map(|(ix, item)| {
        let settled = matches!(
            item.status,
            TodoItemStatus::Done | TodoItemStatus::Cancelled
        );
        let title_color = match item.status {
            TodoItemStatus::InProgress => theme.text,
            TodoItemStatus::Pending | TodoItemStatus::Unknown => theme.text_dim,
            TodoItemStatus::Done | TodoItemStatus::Cancelled => theme.text_faint,
        };
        let glyph = status_glyph(item.status, live, theme, view, cx);
        // A slot inside its flip window pops the new glyph — fade + 2px
        // rise — once; the id keys the flip's status, so each flip is a
        // fresh clock and a settled card never replays.
        let glyph: AnyElement = if flips.get(ix).copied().unwrap_or(false) {
            glyph
                .with_animation(
                    SharedString::from(format!("{row_id}#g{ix}-{:?}", item.status)),
                    FLIP.animation(),
                    |el, t| el.relative().opacity(t).top(px(2.0 * (1.0 - t))),
                )
                .into_any_element()
        } else {
            glyph.into_any_element()
        };
        let mut title = div()
            .min_w_0()
            .flex_1()
            .truncate()
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(title_color)
            .child(SharedString::from(item.title.clone()));
        if settled {
            title = title.line_through();
        }
        div()
            .w_full()
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .min_w_0()
            .border_t_1()
            .border_color(theme.hairline(0.08))
            .child(glyph)
            .child(title)
    });

    let card = div()
        .w_full()
        .max_w(px(720.0))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.hairline(0.12))
        .child(header)
        .child(strip)
        .child(div().flex().flex_col().children(rows).pb(px(4.0)));
    // The entrance plays on the card's first render only — a rewrite
    // re-renders in place and must not re-fade.
    let card = if fresh {
        motion::fade_in(SharedString::from(format!("{row_id}#in")), card).into_any_element()
    } else {
        card.into_any_element()
    };
    div().py(px(4.0)).w_full().child(card).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(row: &str, statuses: &[TodoItemStatus], fraction: f32) -> (Vec<bool>, f32, bool) {
        motion_frame(row, statuses, fraction)
    }

    #[test]
    fn the_first_render_is_static_and_a_flip_pops_once() {
        reset_motion();
        // First sight: no flips armed, the strip renders at its fraction.
        let (flips, shown, animating) = frame(
            "r1",
            &[TodoItemStatus::Pending, TodoItemStatus::Pending],
            0.0,
        );
        assert_eq!(flips, [false, false]);
        assert_eq!(shown, 0.0);
        assert!(!animating);

        // A status change arms exactly that slot.
        let (flips, shown, animating) =
            frame("r1", &[TodoItemStatus::Done, TodoItemStatus::Pending], 0.5);
        assert_eq!(flips, [true, false]);
        assert!(animating);
        assert!((0.0..0.5).contains(&shown), "mid-glide: {shown}");

        // An immediate re-render keeps the flip inside its window; the
        // glide continues.
        let (flips, _, animating) =
            frame("r1", &[TodoItemStatus::Done, TodoItemStatus::Pending], 0.5);
        assert_eq!(flips, [true, false]);
        assert!(animating);
    }

    #[test]
    fn a_settled_glide_renders_the_target_and_stops_pumping() {
        reset_motion();
        frame("r2", &[TodoItemStatus::Pending], 0.0);
        frame("r2", &[TodoItemStatus::Done], 1.0);
        // Past the glide: exactly the target, no frames owed. The window
        // is wall-clock; simulate by writing the clock back.
        TODO_MOTION.with(|motion| {
            let mut motion = motion.borrow_mut();
            let row = motion.rows.get_mut("r2").unwrap();
            row.gliding_since = Some(Instant::now() - GLIDE.total());
            row.flips.clear();
        });
        let (flips, shown, animating) = frame("r2", &[TodoItemStatus::Done], 1.0);
        assert_eq!(flips, [false]);
        assert_eq!(shown, 1.0);
        assert!(!animating);
    }

    #[test]
    fn reseeding_after_a_prune_never_replays() {
        reset_motion();
        frame("r3", &[TodoItemStatus::Done], 1.0);
        reset_motion();
        // A fresh store reseeds from current state: no flip, no glide.
        let (flips, shown, animating) = frame("r3", &[TodoItemStatus::Done], 1.0);
        assert_eq!(flips, [false]);
        assert_eq!(shown, 1.0);
        assert!(!animating);
    }

    #[test]
    fn added_items_are_entrances_not_flips() {
        reset_motion();
        frame("r4", &[TodoItemStatus::Pending], 0.0);
        let (flips, _, _) = frame(
            "r4",
            &[
                TodoItemStatus::Pending,
                TodoItemStatus::Pending,
                TodoItemStatus::Done,
            ],
            1.0 / 3.0,
        );
        assert_eq!(flips, [false, false, false]);
    }

    #[gpui::test]
    fn the_card_draws_every_status(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let theme = Theme::default();
        let items = vec![
            TodoCardItem {
                title: "Map the fold path".into(),
                status: TodoItemStatus::Done,
            },
            TodoCardItem {
                title: "Ship the adapter".into(),
                status: TodoItemStatus::InProgress,
            },
            TodoCardItem {
                title: "Write the tests".into(),
                status: TodoItemStatus::Pending,
            },
            TodoCardItem {
                title: "Drop the footer rework".into(),
                status: TodoItemStatus::Cancelled,
            },
        ];
        // Live and settled: every glyph paints. (A mid-animation frame — the
        // entrance, a flip — needs a real view stack: gpui's animation
        // element requests frames against `current_view`, which a bare
        // `draw` has none of. The flip/glide math is unit-tested above.)
        for live in [true, false] {
            cx.draw(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(800.0), gpui::px(600.0)),
                |_, cx| {
                    card_element(
                        "row-1",
                        &items,
                        live,
                        &[false; 4],
                        0.25,
                        false,
                        &theme,
                        None,
                        cx,
                    )
                },
            );
        }
    }
}
