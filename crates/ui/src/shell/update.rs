//! App self-update: the sidebar disc beside Settings and its dialog. The
//! engine owns the flow (`UpdateStatus` phases); the dialog only confirms the
//! download, shows progress, offers Cancel while bytes are still moving, and
//! relaunches on Restart once the new bundle is in place.

use gpui::relative;
use holt_proto::UpdatePhase;

use super::*;
use crate::motion::{EASE_IN_OUT, MotionSpec};
use crate::theme::ink;

/// One sweep of the indeterminate progress bar.
const UPDATE_SWEEP: MotionSpec = MotionSpec::new(1400, EASE_IN_OUT);
/// Width of the indeterminate bar's moving segment, as a track fraction.
const SWEEP_WIDTH: f32 = 0.32;

impl Shell {
    /// Disc beside the settings row, shown only while the engine reports a
    /// newer release. Its face tracks the phase: download glyph, a progress
    /// ring while downloading, a spinner while installing, then an up arrow
    /// asking for the restart. Every face opens the dialog.
    pub(super) fn render_update_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let update = self.state.read(cx).update.clone();
        let version = update.available.clone()?;
        let (face, accent_fill, tooltip): (AnyElement, bool, SharedString) = match update.phase {
            UpdatePhase::Idle => (
                icon(icons::DOWNLOAD_MINIMALISTIC)
                    .size(px(14.0))
                    .text_color(theme.on_accent)
                    .into_any_element(),
                true,
                format!("Update to Holt {version}").into(),
            ),
            UpdatePhase::Downloading => {
                let face = match download_fraction(update.downloaded, update.total) {
                    Some(frac) => div()
                        .relative()
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            loaders::progress_ring(frac, 24.0, 2.0, ink(0.10), theme.accent)
                                .absolute()
                                .inset_0(),
                        )
                        .child(
                            icon(icons::ARROW_DOWN)
                                .size(px(11.0))
                                .text_color(theme.accent),
                        )
                        .into_any_element(),
                    None => loaders::mini_mono_spinner(
                        "update-spinner",
                        2.0,
                        theme.accent,
                        cx.entity_id(),
                        cx,
                    )
                    .into_any_element(),
                };
                (face, false, format!("Downloading Holt {version}…").into())
            }
            UpdatePhase::Installing => (
                loaders::mini_mono_spinner("update-spinner", 2.0, theme.accent, cx.entity_id(), cx)
                    .into_any_element(),
                false,
                format!("Installing Holt {version}…").into(),
            ),
            UpdatePhase::Ready => (
                icon(icons::ARROW_UP)
                    .size(px(14.0))
                    .text_color(theme.on_accent)
                    .into_any_element(),
                true,
                "Restart to update".into(),
            ),
        };
        Some(
            div()
                .id("sidebar-update")
                .flex_none()
                .size(px(24.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .map(|el| {
                    if accent_fill {
                        el.bg(theme.accent)
                            .hover(|style| style.bg(theme.accent_strong))
                    } else {
                        el.bg(ink(0.04)).hover(|style| style.bg(ink(0.08)))
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_dialog = true;
                    cx.notify();
                }))
                .tooltip(move |_, cx| {
                    cx.new(|_| crate::popover::TextTooltip(tooltip.clone()))
                        .into()
                })
                .child(face)
                .into_any_element(),
        )
    }

    /// The update dialog: confirm → progress (cancellable) → restart.
    /// Clicking outside only hides it; a running download keeps going and
    /// the sidebar disc keeps showing its progress.
    pub(super) fn render_update_dialog(
        &mut self,
        theme: &Theme,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.update_dialog {
            return None;
        }
        let update = self.state.read(cx).update.clone();
        let Some(version) = update.available.clone() else {
            self.update_dialog = false;
            return None;
        };
        let (glyph, title) = match update.phase {
            UpdatePhase::Idle => (icons::DOWNLOAD_MINIMALISTIC, "Update available"),
            UpdatePhase::Downloading => (icons::DOWNLOAD_MINIMALISTIC, "Downloading update"),
            UpdatePhase::Installing => (icons::DOWNLOAD_MINIMALISTIC, "Installing update"),
            UpdatePhase::Ready => (icons::CHECK, "Ready to restart"),
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .bg(theme.accent_wash)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(glyph).size(px(18.0)).text_color(theme.accent)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(popover::dialog_title(theme, title))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .font_family(theme.font_mono.clone())
                            .child(
                                div()
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(update.current_version.clone())),
                            )
                            .child(
                                icon(icons::ARROW_RIGHT)
                                    .size(px(11.0))
                                    .text_color(theme.text_faint),
                            )
                            .child(
                                div()
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(version.clone())),
                            ),
                    ),
            );

        let body = match update.phase {
            UpdatePhase::Idle => div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(popover::dialog_body(
                    theme,
                    format!(
                        "Holt {version} is ready to download. You can keep working while it downloads; Holt restarts only when you say so."
                    ),
                ))
                .when_some(update.error.clone(), |el, error| {
                    el.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(8.0))
                            .rounded(px(8.0))
                            .bg(theme.danger.opacity(0.08))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(17.0))
                            .text_color(theme.danger)
                            .child(
                                icon(icons::DANGER_TRIANGLE)
                                    .flex_none()
                                    .mt(px(1.0))
                                    .size(px(13.0))
                                    .text_color(theme.danger),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(SharedString::from(format!("Last attempt failed: {error}"))),
                            ),
                    )
                }),
            UpdatePhase::Downloading => {
                let frac = download_fraction(update.downloaded, update.total);
                let left = match update.total {
                    Some(total) => format!(
                        "{} of {}",
                        format_bytes(update.downloaded),
                        format_bytes(total)
                    ),
                    None => format_bytes(update.downloaded),
                };
                let right = frac
                    .map(|frac| format!("{}%", (frac * 100.0).floor() as u32))
                    .unwrap_or_default();
                self.progress_block(theme, frac, left, right, cx)
            }
            UpdatePhase::Installing => self.progress_block(
                theme,
                None,
                "Verifying and installing…".into(),
                String::new(),
                cx,
            ),
            UpdatePhase::Ready => div().child(popover::dialog_body(
                theme,
                format!("Holt {version} is installed. Restart Holt to finish updating."),
            )),
        };

        let footer = div()
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(8.0))
            .map(|el| match update.phase {
                UpdatePhase::Idle => el
                    .child(
                        popover::btn_ghost(theme, "Later", "update-later")
                            .id("update-later")
                            .on_click(cx.listener(Self::hide_update_dialog)),
                    )
                    .child(
                        popover::btn_primary(
                            theme,
                            if update.error.is_some() {
                                "Try again"
                            } else {
                                "Download"
                            },
                        )
                        .id("update-download")
                        .on_click(cx.listener(|this, _, _, cx| this.apply_update(cx))),
                    ),
                UpdatePhase::Downloading => el
                    .child(
                        popover::btn_ghost(theme, "Cancel", "update-cancel")
                            .id("update-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_update(cx))),
                    )
                    .child(
                        popover::btn_ghost(theme, "Hide", "update-hide")
                            .id("update-hide")
                            .on_click(cx.listener(Self::hide_update_dialog)),
                    ),
                UpdatePhase::Installing => el.child(
                    popover::btn_ghost(theme, "Hide", "update-hide")
                        .id("update-hide")
                        .on_click(cx.listener(Self::hide_update_dialog)),
                ),
                UpdatePhase::Ready => el
                    .child(
                        popover::btn_ghost(theme, "Later", "update-later")
                            .id("update-later")
                            .on_click(cx.listener(Self::hide_update_dialog)),
                    )
                    .child(
                        popover::btn_primary(theme, "Restart now")
                            .id("update-restart")
                            .on_click(cx.listener(|_, _, _, cx| cx.restart())),
                    ),
            });

        let card = popover::dialog_card(theme)
            .debug_selector(|| "update-dialog".into())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.update_dialog = false;
                cx.notify();
            }))
            .child(header)
            .child(div().mt(px(16.0)).child(body))
            .child(div().mt(px(20.0)).child(footer))
            .into_any_element();
        Some(popover::modal("update-dialog", viewport, card))
    }

    /// A 6px progress track plus its meta row. `frac: None` sweeps an
    /// indeterminate segment (static half-tint under reduced motion).
    fn progress_block(
        &self,
        theme: &Theme,
        frac: Option<f32>,
        left: String,
        right: String,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let fill = match frac {
            Some(frac) => div()
                .h_full()
                .rounded_full()
                .bg(theme.accent)
                .w(relative(frac.max(0.02))),
            None if self.reduced_motion => div()
                .h_full()
                .w_full()
                .rounded_full()
                .bg(theme.accent.opacity(0.4)),
            None => {
                let phase = motion::pulse_delta(&UPDATE_SWEEP, cx.entity_id(), cx);
                let eased = UPDATE_SWEEP.progress(phase);
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(-SWEEP_WIDTH + (1.0 + SWEEP_WIDTH) * eased))
                    .w(relative(SWEEP_WIDTH))
                    .rounded_full()
                    .bg(theme.accent)
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .relative()
                    .h(px(6.0))
                    .w_full()
                    .rounded_full()
                    .overflow_hidden()
                    .bg(ink(0.08))
                    .child(fill),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(left))
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .child(SharedString::from(right)),
                    ),
            )
    }

    /// Start the download + install. The dialog follows the phase stream;
    /// a failure surfaces inline there, or as a notice when it's hidden.
    fn apply_update(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::APPLY_UPDATE, serde_json::json!({}))
                .await;
            if let Err(err) = result {
                this.update(cx, |shell, cx| {
                    if !shell.update_dialog {
                        shell.push_holt_notice(
                            HoltNoticeKind::Error,
                            format!("Update failed: {err}").into(),
                            cx,
                        );
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn hide_update_dialog(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.update_dialog = false;
        cx.notify();
    }

    fn cancel_update(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let _ = engine
                .client()
                .call(methods::CANCEL_UPDATE, serde_json::json!({}))
                .await;
        })
        .detach();
    }
}

/// Downloaded share in `[0,1]`, or `None` while the size is unknown.
fn download_fraction(downloaded: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|total| *total > 0)?;
    Some((downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

/// Binary-unit byte count with one decimal: `12.3 MB`.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(85 * 1024 * 1024), "85.0 MB");
    }

    #[test]
    fn download_fraction_needs_a_known_total() {
        assert_eq!(download_fraction(10, None), None);
        assert_eq!(download_fraction(10, Some(0)), None);
        assert_eq!(download_fraction(50, Some(100)), Some(0.5));
        assert_eq!(download_fraction(200, Some(100)), Some(1.0));
    }
}
