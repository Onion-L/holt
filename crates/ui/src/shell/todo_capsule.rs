//! The task-list capsule (issue #47, replaced surface): the Turn's `todo`
//! list as a pinned pill at the top-right of the chat — stacked under the
//! state card — expanding into the checklist card on click. In goal mode
//! (issue #49) the capsule stands down: the state card carries the list as
//! a labeled section, so two similar cards never stack. The transcript
//! keeps the `TodoCard` part as the record and transport (the capsule is a
//! projection of the latest write); the pill retires when the next Turn
//! opens (a user entry follows the card's entry) or on a manual close.
//!
//! Motion: a status flip pops its glyph once and the progress strip glides
//! to the new fraction — both keyed on a transition MEMORY (the last
//! rendered state), so a remount or a store reseed renders statically
//! instead of replaying. The card↔pill swap rides the state card's size
//! morph; reduced motion snaps all of it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, SharedString, div, prelude::*, px};

use holt_doc::{MessagePart, MessageRole, SessionMessageEntry, TodoCardItem, TodoItemStatus};

use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

use super::Shell;
use super::state_card;

/// The status-flip pop: quick fade + 2px rise.
const FLIP: motion::MotionSpec = motion::MotionSpec::new(180, motion::EASE);
/// The progress strip's glide to a new fraction.
const GLIDE: motion::MotionSpec = motion::MotionSpec::new(320, motion::EASE_OUT_EXPO);
/// How long a flip stays animated after it armed; past the window the slot
/// renders statically (a stale armed flip never pops late).
const FLIP_WINDOW: Duration = Duration::from_millis(260);
/// A card unrendered this long drops its motion memory; the next render
/// reseeds from the current state, so nothing replays.
const STORE_KEEP: Duration = Duration::from_secs(30);

/// The capsule's read of the transcript: the latest write's card id and
/// items, and whether its Turn is still running. Fields are `pub(super)`:
/// the goal card embeds the same list as a section in goal mode (issue #49).
pub(super) struct TodoCapsule {
    pub(super) card_id: String,
    pub(super) items: Vec<TodoCardItem>,
    pub(super) live: bool,
}

/// The capsule's data: the LATEST `TodoCard` part in the chat's
/// transcript, retired once a new Turn (a user entry) follows it — the
/// settled capsule lingers after its own Turn settles, exactly until then.
pub(super) fn current_todo(transcript: &[SessionMessageEntry]) -> Option<TodoCapsule> {
    for entry in transcript.iter().rev() {
        if let Some(part) = entry
            .parts
            .iter()
            .rev()
            .find(|part| matches!(part, MessagePart::TodoCard { .. }))
        {
            let MessagePart::TodoCard { id, items } = part else {
                unreachable!();
            };
            return (!items.is_empty()).then(|| TodoCapsule {
                card_id: format!("{}#{}", entry.id, id),
                items: items.clone(),
                live: entry.status == Some(holt_doc::MessageStatus::Streaming),
            });
        }
        if entry.role == MessageRole::User {
            return None;
        }
    }
    None
}

/// The pill's trailing label: the task in flight, else the next one up.
/// `None` when nothing is open — the count alone carries a settled list.
fn current_title(items: &[TodoCardItem]) -> Option<&str> {
    items
        .iter()
        .find(|item| item.status == TodoItemStatus::InProgress)
        .or_else(|| {
            items
                .iter()
                .find(|item| item.status == TodoItemStatus::Pending)
        })
        .map(|item| item.title.as_str())
}

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

/// What the capsule renders this frame: which slots are inside their flip
/// window, the progress fraction to display, and whether any tween still
/// needs frames. Detects transitions against the store and records them —
/// one call per capsule render. Shared with the goal card's todo section.
pub(super) fn motion_frame(
    card_id: &str,
    statuses: &[TodoItemStatus],
    fraction: f32,
) -> (Vec<bool>, f32, bool) {
    TODO_MOTION.with(|motion| {
        let now = Instant::now();
        let mut motion = motion.borrow_mut();
        let row = motion
            .rows()
            .entry(card_id.to_string())
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

/// One item's status glyph at the capsule's scale — the state card's
/// step-row language: a hairline ring for pending, an accent ring with a
/// breathing dot for in-progress (the shared pulse clock while the Turn is
/// live), a plain green check for done, a dash for cancelled. Shared with
/// the goal card's todo section.
pub(super) fn status_glyph(
    status: TodoItemStatus,
    live: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut App,
) -> gpui::Div {
    let frame = div()
        .flex_none()
        .w(px(16.0))
        .flex()
        .items_center()
        .justify_center();
    match status {
        TodoItemStatus::Pending | TodoItemStatus::Unknown => frame.child(
            div()
                .size(px(10.0))
                .rounded_full()
                .border_1()
                .border_color(theme.hairline(0.28)),
        ),
        TodoItemStatus::InProgress => {
            // The breathing dot rides the shared pulse clock while the Turn
            // is live; a settled Turn holds it lit.
            let dot_opacity = if live {
                0.35 + 0.65 * motion::pulse_wave(motion::pulse_delta(&motion::DOT_PULSE, view, cx))
            } else {
                1.0
            };
            frame.child(
                div()
                    .size(px(11.0))
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
            crate::icons::icon(crate::icons::CHECK)
                .size(px(12.0))
                .text_color(crate::theme::AccentColor::Green.primary(theme.appearance)),
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

/// One row of the list: the status glyph (popping once when its slot just
/// flipped) plus the truncated title. Shared by the capsule's card and the
/// goal card's todo section (issue #49).
pub(super) fn todo_row(
    capsule: &TodoCapsule,
    ix: usize,
    flip: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut App,
) -> AnyElement {
    let item = &capsule.items[ix];
    let settled = matches!(
        item.status,
        TodoItemStatus::Done | TodoItemStatus::Cancelled
    );
    let glyph = status_glyph(item.status, capsule.live, theme, view, cx);
    // A slot inside its flip window pops the new glyph — fade + 2px rise —
    // once; the id keys the flip's status, so each flip is a fresh clock
    // and a settled card never replays.
    let glyph: AnyElement = if flip {
        glyph
            .with_animation(
                SharedString::from(format!("{}#g{ix}-{:?}", capsule.card_id, item.status)),
                FLIP.animation(),
                |el, t| el.relative().opacity(t).top(px(2.0 * (1.0 - t))),
            )
            .into_any_element()
    } else {
        glyph.into_any_element()
    };
    div()
        .h(px(24.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(7.0))
        .child(glyph)
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(13.5))
                .line_height(px(19.0))
                .text_color(match item.status {
                    TodoItemStatus::InProgress => theme.text,
                    TodoItemStatus::Pending | TodoItemStatus::Unknown => theme.text_muted,
                    TodoItemStatus::Done | TodoItemStatus::Cancelled => theme.text_faint,
                })
                .when(settled, |el| el.line_through())
                .child(SharedString::from(item.title.clone())),
        )
        .into_any_element()
}

/// The capsule's progress ring: the done fraction as a 14px arc beside the
/// count. The arc rides the motion frame's displayed fraction, so rewrites
/// glide. Shared with the goal card's todo section.
pub(super) fn todo_ring(displayed: f32, theme: &Theme) -> AnyElement {
    crate::loaders::progress_ring(displayed, 14.0, 1.5, theme.ink(0.12), theme.accent)
        .flex_none()
        .into_any_element()
}

/// The two forms' shared inputs: the capsule's data plus the motion
/// frame's computed view (counts derived once, shared by both).
struct TodoForm<'a> {
    capsule: &'a TodoCapsule,
    flips: &'a [bool],
    done: usize,
    total: usize,
    displayed: f32,
}

impl Shell {
    /// The todo capsule: `None` when the chat has no live task list (never
    /// written, retired by a following Turn, or manually dismissed). The
    /// pill shows count + current task and expands on click; the card
    /// carries the checklist, the collapse chevron, and the close button.
    pub(super) fn render_todo_capsule(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // Goal mode: the state card carries the list as a section (issue
        // #49) — the capsule never stacks a second card under it. Chats
        // without a goal keep the capsule exactly as it was.
        if self
            .state
            .read(cx)
            .selected_chat_row()
            .is_some_and(|chat| chat.goal.is_some())
        {
            return None;
        }
        let capsule = current_todo(&self.state.read(cx).transcript)?;
        if self.todo_capsule_dismissed.as_deref() == Some(capsule.card_id.as_str()) {
            return None;
        }
        let done = capsule
            .items
            .iter()
            .filter(|item| item.status == TodoItemStatus::Done)
            .count();
        let total = capsule.items.len();
        let fraction = done as f32 / total.max(1) as f32;
        let statuses: Vec<TodoItemStatus> = capsule.items.iter().map(|item| item.status).collect();
        let (flips, displayed, animating) = motion_frame(&capsule.card_id, &statuses, fraction);
        if animating {
            // The shell render tail keeps frames coming while a tween is
            // mid-flight.
            self.motion_active.set(true);
        }
        let collapsed = !self.todo_capsule_expanded;
        let form = TodoForm {
            capsule: &capsule,
            flips: &flips,
            done,
            total,
            displayed,
        };
        let (id, content) = if collapsed {
            ("todo-capsule-pill", self.todo_pill(&form, theme))
        } else {
            ("todo-capsule-card", self.todo_card(&form, theme, cx))
        };

        // The card↔pill swap morphs the box between the two forms' measured
        // sizes — the state card's mechanics, one surface below.
        let natural = self.todo_capsule_size.get();
        let morph_size = state_card::morph_box_size(
            &mut self.todo_capsule_morph,
            natural,
            self.reduced_motion,
            &self.motion_active,
        );
        let measured = self.todo_capsule_size.clone();
        let mut card = state_card::state_card_chrome(theme)
            .id(id)
            .when_some(morph_size, |el, (w, h)| el.w(px(w)).h(px(h)))
            .flex()
            .flex_col()
            .items_end()
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                measured.set((
                                    f32::from(bounds.size.width),
                                    f32::from(bounds.size.height),
                                ));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(content),
            );
        if collapsed {
            // The pill itself is the expand button (the whole surface — no
            // competing affordances on it). The tooltip carries the
            // truncated task title.
            let tooltip: SharedString = current_title(&capsule.items)
                .unwrap_or("Task list")
                .to_owned()
                .into();
            card = card
                .debug_selector(|| "todo-capsule-pill".to_string())
                .cursor_pointer()
                .tooltip(move |_, cx| {
                    let tooltip = tooltip.clone();
                    cx.new(|_| crate::popover::TextTooltip(tooltip)).into()
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_todo_capsule_collapsed(false, cx);
                }));
        }
        Some(state_card::fade_and_frost(card, id, 1.0))
    }

    /// The capsule: identity glyph (accent while the Turn is live), the
    /// count, and the current task. The whole pill is the expand button;
    /// dismissal lives in the expanded card, never on the pill.
    fn todo_pill(&mut self, form: &TodoForm, theme: &Theme) -> AnyElement {
        let (done, total) = (form.done, form.total);
        let all_done = done == total;
        let icon_tint = if form.capsule.live {
            theme.accent
        } else {
            theme.text_muted
        };
        let mut label = div()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("{done}/{total}"))),
            );
        if let Some(title) = current_title(&form.capsule.items) {
            label = label.child(
                div()
                    .min_w_0()
                    .max_w(px(200.0))
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(if form.capsule.live {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .child(SharedString::from(title.to_owned())),
            );
        } else if all_done {
            // Nothing open: the count alone reads as a cryptic fraction —
            // say what it means.
            label = label.child(
                div()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child("Done"),
            );
        }
        div()
            .group("todo-capsule-pill")
            .h(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .pl(px(10.0))
            .pr(px(8.0))
            .child(
                crate::icons::icon(crate::icons::CHECKLIST)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(icon_tint),
            )
            .child(label)
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_faint),
            )
            .into_any_element()
    }

    /// The expanded card: header (identity, count, collapse + close), the
    /// progress strip, then one row per item. The flip pop plays on the
    /// glyph of a slot whose status just moved.
    fn todo_card(&mut self, form: &TodoForm, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let capsule = form.capsule;
        let card_id = capsule.card_id.clone();
        let view = cx.entity_id();
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .pb(px(7.0))
            .border_b_1()
            .border_color(theme.hairline(0.08))
            .child(
                crate::icons::icon(crate::icons::CHECKLIST)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(if capsule.live {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(14.0))
                    .line_height(px(20.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Todo"),
            )
            .child(todo_ring(form.displayed, theme))
            .child(
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("{}/{}", form.done, form.total))),
            )
            .child(
                div()
                    .id("todo-capsule-collapse")
                    .debug_selector(|| "todo-capsule-collapse".to_string())
                    .flex_none()
                    .size(px(24.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|el| el.bg(crate::theme::ink(0.06)))
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::popover::TextTooltip("Collapse".into()))
                            .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_todo_capsule_collapsed(true, cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_UP)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .id("todo-capsule-close")
                    .debug_selector(|| "todo-capsule-close".to_string())
                    .flex_none()
                    .size(px(24.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|el| el.bg(crate::theme::ink(0.06)))
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::popover::TextTooltip("Close until the next list".into()))
                            .into()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.todo_capsule_dismissed = Some(card_id.clone());
                        this.todo_capsule_expanded = false;
                        cx.notify();
                    }))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    ),
            );

        let rows = (0..capsule.items.len()).map(|ix| {
            todo_row(
                capsule,
                ix,
                form.flips.get(ix).copied().unwrap_or(false),
                theme,
                view,
                cx,
            )
        });

        div()
            .w(px(state_card::CARD_WIDTH))
            .px(px(12.0))
            .pt(px(10.0))
            .pb(px(12.0))
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .pt(px(7.0))
                    .children(rows),
            )
            .into_any_element()
    }

    /// Card ↔ capsule toggle: pure view chrome (the list keeps updating).
    /// In-memory — a fresh session opens every card collapsed. Starts the
    /// size morph from the outgoing form's measured size.
    fn set_todo_capsule_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.todo_capsule_morph =
            state_card::arm_morph(self.todo_capsule_size.get(), self.reduced_motion);
        self.todo_capsule_expanded = !collapsed;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_doc::MessageStatus;

    fn entry(id: &str, role: MessageRole, status: Option<MessageStatus>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status,
            continuation_of: None,
        }
    }

    fn with_card(
        id: &str,
        status: Option<MessageStatus>,
        items: Vec<(&str, TodoItemStatus)>,
    ) -> SessionMessageEntry {
        let mut entry = entry(id, MessageRole::Assistant, status);
        entry.parts.push(MessagePart::TodoCard {
            id: "call-1-card".into(),
            items: items
                .into_iter()
                .map(|(title, status)| TodoCardItem {
                    title: title.to_string(),
                    status,
                })
                .collect(),
        });
        entry
    }

    #[test]
    fn the_capsule_reads_the_latest_write_and_its_liveness() {
        let transcript = vec![
            entry("u1", MessageRole::User, None),
            entry("r1", MessageRole::Assistant, Some(MessageStatus::Complete)),
            entry("u2", MessageRole::User, None),
            with_card(
                "r2",
                Some(MessageStatus::Streaming),
                vec![
                    ("map", TodoItemStatus::Done),
                    ("ship", TodoItemStatus::InProgress),
                ],
            ),
        ];
        let capsule = current_todo(&transcript).unwrap();
        assert_eq!(capsule.card_id, "r2#call-1-card");
        assert_eq!(capsule.items.len(), 2);
        assert!(capsule.live);

        // Settled Turn: the capsule lingers, no longer live.
        let transcript = vec![
            entry("u1", MessageRole::User, None),
            with_card(
                "r1",
                Some(MessageStatus::Complete),
                vec![("map", TodoItemStatus::Done)],
            ),
        ];
        let capsule = current_todo(&transcript).unwrap();
        assert!(!capsule.live);
    }

    #[test]
    fn a_new_turn_retires_the_previous_card() {
        let transcript = vec![
            entry("u1", MessageRole::User, None),
            with_card(
                "r1",
                Some(MessageStatus::Complete),
                vec![("map", TodoItemStatus::Done)],
            ),
            // The next Turn opened (a user entry) without a write yet.
            entry("u2", MessageRole::User, None),
        ];
        assert!(current_todo(&transcript).is_none());
        assert!(current_todo(&[]).is_none());
        // An empty write renders nothing.
        assert!(current_todo(&[with_card("r1", None, vec![])]).is_none());
    }

    #[test]
    fn the_current_title_prefers_in_progress_then_pending() {
        let items = |statuses: &[TodoItemStatus]| {
            statuses
                .iter()
                .enumerate()
                .map(|(i, status)| TodoCardItem {
                    title: format!("task {i}"),
                    status: *status,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            current_title(&items(&[TodoItemStatus::Done, TodoItemStatus::InProgress])),
            Some("task 1")
        );
        assert_eq!(
            current_title(&items(&[TodoItemStatus::Done, TodoItemStatus::Pending])),
            Some("task 1")
        );
        assert_eq!(
            current_title(&items(&[TodoItemStatus::Done, TodoItemStatus::Cancelled])),
            None
        );
    }

    #[test]
    fn the_first_render_is_static_and_a_flip_pops_once() {
        reset_motion();
        let (flips, shown, animating) = motion_frame(
            "r1",
            &[TodoItemStatus::Pending, TodoItemStatus::Pending],
            0.0,
        );
        assert_eq!(flips, [false, false]);
        assert_eq!(shown, 0.0);
        assert!(!animating);

        let (flips, shown, animating) =
            motion_frame("r1", &[TodoItemStatus::Done, TodoItemStatus::Pending], 0.5);
        assert_eq!(flips, [true, false]);
        assert!(animating);
        assert!((0.0..0.5).contains(&shown), "mid-glide: {shown}");

        // An immediate re-render keeps the flip inside its window.
        let (flips, _, animating) =
            motion_frame("r1", &[TodoItemStatus::Done, TodoItemStatus::Pending], 0.5);
        assert_eq!(flips, [true, false]);
        assert!(animating);
    }

    #[test]
    fn a_settled_glide_renders_the_target_and_stops_pumping() {
        reset_motion();
        motion_frame("r2", &[TodoItemStatus::Pending], 0.0);
        motion_frame("r2", &[TodoItemStatus::Done], 1.0);
        // Past the glide: exactly the target, no frames owed. The window
        // is wall-clock; simulate by writing the clock back.
        TODO_MOTION.with(|motion| {
            let mut motion = motion.borrow_mut();
            let row = motion.rows.get_mut("r2").unwrap();
            row.gliding_since = Some(Instant::now() - GLIDE.total());
            row.flips.clear();
        });
        let (flips, shown, animating) = motion_frame("r2", &[TodoItemStatus::Done], 1.0);
        assert_eq!(flips, [false]);
        assert_eq!(shown, 1.0);
        assert!(!animating);
    }

    #[test]
    fn reseeding_after_a_prune_never_replays() {
        reset_motion();
        motion_frame("r3", &[TodoItemStatus::Done], 1.0);
        reset_motion();
        let (flips, shown, animating) = motion_frame("r3", &[TodoItemStatus::Done], 1.0);
        assert_eq!(flips, [false]);
        assert_eq!(shown, 1.0);
        assert!(!animating);
    }

    #[test]
    fn added_items_are_entrances_not_flips() {
        reset_motion();
        motion_frame("r4", &[TodoItemStatus::Pending], 0.0);
        let (flips, _, _) = motion_frame(
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

    /// Goal mode (issue #49): the state card carries the list as a section,
    /// so the capsule never stacks under it — a goal chat with a live list
    /// draws nothing here. (The section's own test lives in state_card.rs.)
    #[gpui::test]
    fn goal_mode_suppresses_the_capsule(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;

        use crate::state::AppState;

        struct NoopEngine;

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for NoopEngine {
            async fn handle(
                &self,
                method: &str,
                _params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "goal-chat", "deviceId": "test-device", "archived": false,
                    "cwd": "/tmp", "createdAt": "2026-10-10T00:00:00Z",
                    "goal": {
                        "text": "ship the compaction slice",
                        "status": "active", "iteration": 1,
                        "noProgress": 0, "evalFailures": 0,
                        "startedAt": "2026-10-10T00:00:00Z"
                    }
                }))
                .unwrap(),
            );
            state.selected_chat = Some("goal-chat".into());
            state.transcript = vec![with_card(
                "r1",
                Some(MessageStatus::Streaming),
                vec![("ship", TodoItemStatus::InProgress)],
            )];
            state
        });
        let engine = Arc::new(NoopEngine);
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine)
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = super::super::Route::Chat;
            shell
        });
        shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            assert!(
                shell.render_todo_capsule(&theme, cx).is_none(),
                "a goal chat never draws the standalone capsule"
            );
        });
    }

    /// The drawing + wiring half (the state card tests' pattern): a chat
    /// whose transcript carries a TodoCard draws the pill; a click expands
    /// the card; the close button dismisses it; and the next Turn's user
    /// entry retires it. No RPC fires — the capsule is pure view chrome.
    #[gpui::test]
    fn the_pill_expands_dismisses_and_retires(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;

        use crate::state::AppState;

        struct NoopEngine;

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for NoopEngine {
            async fn handle(
                &self,
                method: &str,
                _params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                Err(holt_rpc::RpcError::UnknownMethod(method.to_string()))
            }
        }

        struct CapsuleView {
            shell: gpui::Entity<Shell>,
        }
        impl gpui::Render for CapsuleView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let theme = Theme::of(cx).clone();
                let capsule = self
                    .shell
                    .update(cx, |shell, cx| shell.render_todo_capsule(&theme, cx));
                div().children(capsule)
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "todo-chat", "deviceId": "test-device", "archived": false,
                    "cwd": "/tmp", "createdAt": "2026-10-10T00:00:00Z",
                }))
                .unwrap(),
            );
            state.selected_chat = Some("todo-chat".into());
            state.transcript = vec![
                SessionMessageEntry {
                    id: "u1".into(),
                    role: MessageRole::User,
                    parts: vec![MessagePart::Text {
                        id: "t0".into(),
                        text: "track this".into(),
                    }],
                    created_at: 0,
                    device_id: "dev".into(),
                    status: None,
                    continuation_of: None,
                },
                with_card(
                    "r1",
                    Some(MessageStatus::Streaming),
                    vec![
                        ("map", TodoItemStatus::Done),
                        ("ship", TodoItemStatus::InProgress),
                    ],
                ),
            ];
            state
        });
        let engine = Arc::new(NoopEngine);
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine)
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let shell = cx.new(|cx| {
            let mut shell = Shell::new(
                state,
                crate::state::EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = super::super::Route::Chat;
            shell
        });
        shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            assert!(
                shell.render_todo_capsule(&theme, cx).is_some(),
                "a chat with a live task list draws the capsule"
            );
        });
        let (_view, visual) = cx.add_window_view(|_, _| CapsuleView {
            shell: shell.clone(),
        });

        // The pill expands on click: the card's rows and controls paint.
        let pill = visual
            .debug_bounds("todo-capsule-pill")
            .expect("the pill renders");
        visual.simulate_click(pill.center(), Default::default());
        let close = visual
            .debug_bounds("todo-capsule-close")
            .expect("the expanded card renders its close button");
        assert!(
            visual.debug_bounds("todo-capsule-collapse").is_some(),
            "the collapse button renders"
        );

        // The close button dismisses: the capsule unmounts, and no new
        // render brings the same card back.
        visual.simulate_click(close.center(), Default::default());
        shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            assert!(shell.render_todo_capsule(&theme, cx).is_none());
        });

        // The next Turn's write is a NEW card: it shows again.
        shell.update(cx, |shell, cx| {
            shell.state.update(cx, |state, _| {
                state.transcript.push(with_card(
                    "r2",
                    Some(MessageStatus::Streaming),
                    vec![("fresh", TodoItemStatus::InProgress)],
                ));
            });
            let theme = Theme::of(cx).clone();
            assert!(
                shell.render_todo_capsule(&theme, cx).is_some(),
                "a new card un-dismisses"
            );
        });
    }
}
