//! Stacked top-right notification chips ("holt notices"). Each entry owns
//! a 2s auto-dismiss timer that pauses while hovered; color/icon come from
//! the notice's semantic kind.

use super::*;

/// Semantic kind for a holt notice — drives the icon + color tokens used
/// to render the chip. Each kind pairs with a `(border/icon, text)` color
/// pair so error reads as urgent, success as affirming, etc.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HoltNoticeKind {
    /// Routine status (e.g. "Copying…", deep-link echoes). Brand-accent
    /// tint — informational but not alarming.
    Plain,
    /// Completed an action the user asked for (copy succeeded).
    Success,
    /// Heads-up that something is wrong or might fail (missing data,
    /// deprecation). Amber tint — softer than error.
    Warning,
    /// Action failed. Red tint — the most attention-demanding kind.
    Error,
}

/// One stacked top-right notification chip — replaces the inline sidebar
/// notice strip. Each entry owns its own auto-dismiss timer; the timer is
/// dropped (canceled) while hovered and re-armed on unhover. Manual
/// dismissal is also exposed via the close button.
pub(super) struct HoltNotice {
    /// Stable per-notice id — lets listeners and animations key on the
    /// specific entry even when several stack at once.
    pub(super) id: u64,
    pub(super) kind: HoltNoticeKind,
    pub(super) message: SharedString,
    /// Whether the pointer is currently over the chip. Drives the
    /// hover-pause contract for the auto-dismiss timer.
    pub(super) hovered: bool,
    /// 2s auto-dismiss. Dropped (and therefore canceled) when the chip
    /// becomes hovered, re-armed on unhover, and on manual close.
    pub(super) timer: Option<Task<()>>,
}

/// Color pair used to paint a holt notice chip: `(border/icon, text)`.
/// Kept local to the shell since the pairing is a chip-specific decision,
/// not a generic theme contract.
fn holt_notice_palette(kind: HoltNoticeKind, theme: &Theme) -> (gpui::Hsla, gpui::Hsla) {
    match kind {
        // Plain uses the brand accent — informational without alarm.
        HoltNoticeKind::Plain => (theme.accent, theme.accent.opacity(0.85)),
        HoltNoticeKind::Success => (theme.success, theme.success_muted),
        HoltNoticeKind::Warning => (theme.warning, theme.warning_muted),
        HoltNoticeKind::Error => (theme.danger, theme.danger_muted),
    }
}

/// Icon glyph for each notice kind. Warning and error share the triangle
/// so the *color* — not the shape — carries the severity distinction
/// (matches the rest of the app, e.g. provider_error).
fn holt_notice_icon(kind: HoltNoticeKind) -> &'static str {
    match kind {
        HoltNoticeKind::Plain => icons::INFO_CIRCLE,
        HoltNoticeKind::Success => icons::CHECK,
        HoltNoticeKind::Warning => icons::DANGER_TRIANGLE,
        HoltNoticeKind::Error => icons::DANGER_TRIANGLE,
    }
}

impl Shell {
    /// Push a top-right holt notice and arm its 2s auto-dismiss timer.
    /// Multiple notices stack vertically and coexist; each owns its own
    /// timer. Replaces the inline `sidebar_notice` strip.
    pub(super) fn push_holt_notice(
        &mut self,
        kind: HoltNoticeKind,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_holt_notice_id;
        self.next_holt_notice_id += 1;
        self.holt_notices.push(HoltNotice {
            id,
            kind,
            message,
            hovered: false,
            timer: None,
        });
        self.arm_holt_notice_timer(id, cx);
        cx.notify();
    }

    /// (Re)arm the auto-dismiss timer for a single notice. The task
    /// captures the notice id; on fire it removes that exact entry — other
    /// stacked notices are left alone.
    fn arm_holt_notice_timer(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(notice) = self.holt_notices.iter_mut().find(|n| n.id == id) {
            notice.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(pos) = this.holt_notices.iter().position(|n| n.id == id) {
                        this.holt_notices.remove(pos);
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
    }

    /// Hover state flip for one chip — pauses its timer while hovered
    /// and rearms it once the pointer leaves. `false` is delivered when
    /// the element goes away (including via timer fire); the lookup is
    /// guarded so a missing entry is a no-op.
    fn set_holt_notice_hover(&mut self, id: u64, hovered: bool, cx: &mut Context<Self>) {
        let needs_rearm = if let Some(notice) = self.holt_notices.iter_mut().find(|n| n.id == id) {
            if notice.hovered == hovered {
                return;
            }
            notice.hovered = hovered;
            if hovered {
                // Drop the task to cancel it (no epoch guard needed).
                notice.timer = None;
                false
            } else {
                true
            }
        } else {
            return;
        };
        if needs_rearm {
            self.arm_holt_notice_timer(id, cx);
        }
        cx.notify();
    }

    /// Manual dismiss for one chip (the × button). Removes the entry and
    /// drops its timer; other notices are untouched.
    fn dismiss_holt_notice(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(pos) = self.holt_notices.iter().position(|n| n.id == id) {
            self.holt_notices.remove(pos);
            cx.notify();
        }
    }

    /// Render one stacked top-right notice chip. Color and icon are
    /// driven by `notice.kind` so success / error / etc. read at a glance.
    /// Returns `AnyElement` — the outer caller wraps it in
    /// `motion::dialog_in` for the per-chip entrance animation.
    pub(super) fn render_holt_notice(
        &self,
        notice: &HoltNotice,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = notice.id;
        let (accent, text) = holt_notice_palette(notice.kind, theme);
        let glyph = holt_notice_icon(notice.kind);
        div()
            .id(("holt-notice-card", id))
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .max_w(px(420.0))
            .pl(px(14.0))
            .pr(px(8.0))
            .py(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(accent.opacity(0.35))
            .bg(theme.surface_dialog)
            .shadow_lg()
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(text)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_holt_notice_hover(id, *hovered, cx);
            }))
            .child(icon(glyph).size(px(15.0)).flex_none().text_color(accent))
            // min_w(0) beats flex `min-width: auto` — gpui measures text
            // min-content as the full unwrapped width, so without it a long
            // unbreakable URL cannot shrink and overflows past the chip.
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(notice.message.clone()),
            )
            .child(
                div()
                    .id(("holt-notice-dismiss", id))
                    .flex_none()
                    .cursor_pointer()
                    .p(px(4.0))
                    .rounded(px(6.0))
                    .hover(|style| style.bg(crate::theme::ink(0.08)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dismiss_holt_notice(id, cx);
                    }))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }
}
