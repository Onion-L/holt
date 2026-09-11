//! Shared scaffolding for the settings pages — the original's page rhythm
//! (`mx-auto max-w-3xl px-6 pb-16 pt-8`), section headings, row layout, badges
//! and small buttons, so every page reads as the same product surface
//! (holt settings.agents.tsx / settings.archived.tsx). A few cross-surface
//! controls (the checkbox) live here too, following the same display-only
//! idiom.

use gpui::{AnyElement, SharedString, div, prelude::*, px};

use crate::{
    motion::{self, AnimationExt as _},
    theme::{Theme, ink},
};

/// Shared typography for a settings component's title and description. The
/// Shortcuts page established this compact rhythm; list-style settings reuse
/// it instead of drifting by page.
pub const ROW_TITLE_SIZE: f32 = 13.0;
pub const ROW_DESCRIPTION_SIZE: f32 = 12.0;

/// Centered page column: `mx-auto w-full max-w-[960px] px-6 pb-16 pt-8`.
pub fn page_column() -> gpui::Div {
    div()
        .w_full()
        .max_w(px(960.0))
        .mx_auto()
        .px(px(24.0))
        .pt(px(32.0))
        .pb(px(64.0))
        .flex()
        .flex_col()
}

/// Page headline row: `flex items-baseline gap-2.5` — `text-base font-semibold`
/// title + `text-[13px]` count sharing a baseline (holt settings.agents.tsx).
pub fn page_header(theme: &Theme, title: &str, count: Option<usize>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(10.0))
        .child(
            div()
                .text_size(crate::typography::ui_rems(16.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .when_some(count, |el, count| {
            el.child(
                div()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(format!("{count}"))),
            )
        })
}

/// Subtitle under the headline: `mt-1 text-[13px] text-muted-foreground`.
pub fn page_subtitle(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt(px(4.0))
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(theme.text_muted)
        .child(copy.into())
}

/// Small label above a group of controls (`text-[13px] font-medium`) — the
/// "Theme" caption over a picker, not a page headline.
pub fn field_label(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(label.into())
}

/// Corner radius of the miniature preview frames (the theme-import dialog's
/// scene preview).
///
/// Public because the preview has to round *itself* to this. gpui content masks
/// are axis-aligned rectangles, so `overflow_hidden` on the frame clips to its
/// bounding box and not to its corner radius — a preview that paints its own
/// background will square off the corners and cover the frame's border with it.
pub const OPTION_CARD_RADIUS: f32 = 6.0;

/// A settings row rendered directly on the page — no card, no separators:
/// title + description on the left, the control on the right, with vertical
/// rhythm alone separating the rows.
pub fn flat_row() -> gpui::Div {
    div()
        .w_full()
        .py(px(14.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(24.0))
}

/// The muted description line under a [`flat_row`]'s title.
pub fn row_description(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(ROW_DESCRIPTION_SIZE))
        .line_height(px(18.0))
        .text_color(theme.text_muted)
        .child(copy.into())
}

/// Section caption above a group's rows ("Notifications", "Web search"):
/// 14px semibold — between the 16px page headline and the 13px row titles,
/// so the group's name reads as a heading and never as another row title.
pub fn section_label(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(14.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(label.into())
}

/// The identity tile on a row: `size-9 rounded-[10px] border bg-white/[0.03]`
/// around a 16px icon.
pub fn row_tile(theme: &Theme, icon_path: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .size(px(36.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border)
        .bg(ink(0.03))
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(icon_path)
                .size(px(16.0))
                .text_color(theme.text_muted),
        )
}

/// Row title. These metrics intentionally match the Shortcuts rows, whose
/// title/description rhythm is the reference for the other settings cards.
pub fn row_title(theme: &Theme, title: impl Into<SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(crate::typography::ui_rems(ROW_TITLE_SIZE))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(title.into())
}

/// The quiet meta line under a row title: `text-[12px]
/// text-muted-foreground/65` fragments joined by dots.
pub fn meta_line(theme: &Theme, fragments: Vec<AnyElement>) -> gpui::Div {
    let mut line = div()
        .mt(px(Theme::TEXT_STACK_GAP))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .gap_y(px(2.0))
        .text_size(crate::typography::ui_rems(ROW_DESCRIPTION_SIZE))
        .text_color(theme.text_muted.opacity(0.65));
    let mut first = true;
    for fragment in fragments {
        if !first {
            line = line.child(
                div()
                    .text_color(theme.text_muted.opacity(0.3))
                    .child(SharedString::from("·")),
            );
        }
        line = line.child(fragment);
        first = false;
    }
    line
}

/// Right-anchored badge pill: `rounded-full border px-2 py-0.5 text-[10.5px]`.
pub fn badge(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_size(crate::typography::ui_rems(10.5))
        .text_color(theme.text_muted)
        .child(label.into())
}

/// Emerald status pill (the Accounts "Active" badge:
/// `bg-emerald-400/[0.12] text-emerald-300/90`).
pub fn badge_active(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    let emerald = theme.success;
    let emerald_text = theme.success_muted; // emerald-300
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .bg(emerald.opacity(0.12))
        .text_size(crate::typography::ui_rems(10.5))
        .text_color(emerald_text.opacity(0.9))
        .child(label.into())
}

/// Display-only toggle switch (holt branch-picker.tsx `Toggle`): an 18×32
/// pill whose knob slides right and track flips white when on. State is owned
/// by the parent row — the caller adds `.id(..)` and `.on_click(..)`.
pub fn toggle_switch(theme: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .bg(if on { theme.text } else { ink(0.15) })
        .relative()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if on { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if on { theme.on_solid } else { ink(0.7) }),
        )
}

/// Toggle switch transitioning between two committed states. The outer hit
/// target keeps fixed geometry, so the animation never moves surrounding rows.
pub fn animated_toggle_switch(
    theme: &Theme,
    on: bool,
    animation_key: impl Into<SharedString>,
) -> gpui::Div {
    let animation_key = animation_key.into();
    let from_x = if on { 2.0 } else { 16.0 };
    let to_x = if on { 16.0 } else { 2.0 };
    let from_track = if on { ink(0.15) } else { theme.text };
    let to_track = if on { theme.text } else { ink(0.15) };
    let from_knob = if on { ink(0.7) } else { theme.on_solid };
    let to_knob = if on { theme.on_solid } else { ink(0.7) };
    let knob = div()
        .absolute()
        .top(px(2.0))
        .size(px(14.0))
        .rounded_full()
        .with_animation(
            SharedString::from(format!("{animation_key}-knob")),
            motion::TOGGLE.animation(),
            move |knob, progress| {
                knob.left(px(motion::lerp(from_x, to_x, progress)))
                    .bg(motion::mix(from_knob, to_knob, progress))
            },
        );
    let track = div()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .relative()
        .child(knob)
        .with_animation(
            SharedString::from(format!("{animation_key}-track")),
            motion::TOGGLE.animation(),
            move |track, progress| track.bg(motion::mix(from_track, to_track, progress)),
        );
    div().flex_none().w(px(32.0)).h(px(18.0)).child(track)
}

/// Checkbox state for [`checkbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckboxState {
    Unchecked,
    Checked,
    /// Visible but never clickable (a conflicted status row, a control over
    /// a non-git root).
    Disabled,
}

/// Display-only checkbox (the Git panel's staging rows): a 14px rounded box,
/// accent-filled with a check glyph when on, dimmed when disabled. State is
/// owned by the parent row — the caller adds `.id(..)` and `.on_click(..)`.
pub fn checkbox(theme: &Theme, state: CheckboxState) -> gpui::Div {
    let mut box_el = div()
        .flex_none()
        .size(px(14.0))
        .rounded(px(4.0))
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    box_el = match state {
        CheckboxState::Checked => box_el.bg(theme.accent).border_color(theme.accent),
        _ => box_el.border_color(theme.border_strong).bg(ink(0.02)),
    };
    if state == CheckboxState::Checked {
        box_el = box_el.child(
            crate::icons::icon(crate::icons::CHECK)
                .size(px(10.0))
                .text_color(theme.on_solid),
        );
    }
    if state == CheckboxState::Disabled {
        box_el = box_el.opacity(0.4);
    }
    box_el
}

/// A small quiet ghost action (`rounded-lg px-2.5 py-1.5 text-[12px]
/// text-muted-foreground`). Caller adds id + click + leading icon child AND
/// its own `.hover(..)` — gpui panics on a second hover, and the pages vary
/// it (reveal opacity, 4% vs 6% washes).
pub fn ghost_action(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .rounded(px(8.0))
        .px(px(10.0))
        .py(px(6.0))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted)
        .cursor_pointer()
}

/// The default ghost-action hover wash (`hover:bg-white/[0.06]
/// hover:text-foreground`).
pub fn ghost_hover(theme: &Theme, s: gpui::StyleRefinement) -> gpui::StyleRefinement {
    s.bg(ink(0.06)).text_color(theme.text)
}

/// The dismissible red error strip (`flex items-start gap-2 rounded-xl border
/// border-red-400/20 bg-red-400/[0.06] text-red-300/90` with a leading
/// `DangerTriangle mt-0.5 size-4`).
pub fn error_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    let red = theme.danger; // red-400
    let red_text = theme.danger_muted; // red-300
    div()
        .mt(px(16.0))
        .px(px(16.0))
        .py(px(12.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(red.opacity(0.2))
        .bg(red.opacity(0.06))
        .text_size(crate::typography::ui_rems(12.5))
        .text_color(red_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(16.0))
                    .text_color(red_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}

/// The amber warning strip (`flex items-start gap-2 border-amber-400/20
/// bg-amber-400/[0.06] text-amber-200/90` with a leading `DangerTriangle
/// mt-0.5 size-3.5`).
pub fn warning_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    let amber = theme.warning; // amber-400
    let amber_text = theme.warning_muted; // amber-200
    div()
        .mt(px(8.0))
        .px(px(16.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(amber.opacity(0.2))
        .bg(amber.opacity(0.06))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(amber_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(14.0))
                    .text_color(amber_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}
