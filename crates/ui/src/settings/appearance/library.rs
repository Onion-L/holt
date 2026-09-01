//! The custom-theme library rows and the theme-mapping review dialog.

use super::import::{import_scene_preview, report_panel};
use super::*;

impl AppearancePage {
    pub(super) fn render_review_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entry_id = self.review_entry.as_ref()?;
        let entry = theme_library::entries(cx)
            .into_iter()
            .find(|entry| &entry.id == entry_id)?;
        let mut card = popover::dialog_card(theme)
            .id("theme-review-card")
            .w(px(660.0))
            .max_h(px(720.0))
            .overflow_y_scroll()
            .child(popover::dialog_title(theme, "Theme mapping"))
            .child(
                popover::dialog_body(theme, format!("{} · {}", entry.name, entry.source.label()))
                    .mt(px(6.0)),
            );
        for variant in &entry.family.variants {
            card = card
                .child(
                    div()
                        .mt(px(14.0))
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(SharedString::from(variant.name.clone())),
                )
                .child(import_scene_preview(variant));
            if let Some(report) = entry.reports.get(&variant.id) {
                card = card.child(report_panel(theme, report));
            }
        }
        card = card.child(
            div().mt(px(16.0)).flex().justify_end().child(
                popover::btn_primary(theme, "Done")
                    .id("theme-review-close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.review_entry = None;
                        cx.notify();
                    })),
            ),
        );
        Some(popover::modal(
            "theme-review-dialog",
            viewport,
            card.into_any_element(),
        ))
    }

    pub(super) fn render_library_entry(
        &mut self,
        entry: CustomThemeEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = entry.id.clone();
        let linked = entry.source.is_linked();
        let source = entry
            .source
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Self-contained snapshot".into());
        let status = match &entry.status {
            CustomThemeStatus::Ready => format!(
                "{} · {} variant{} · {}",
                entry.source.label(),
                entry.family.variants.len(),
                if entry.family.variants.len() == 1 {
                    ""
                } else {
                    "s"
                },
                source
            ),
            CustomThemeStatus::Warning { message } => {
                format!("Using last known good · {message}")
            }
        };
        widgets::flat_row()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::row_title(theme, &entry.name))
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(
                                if matches!(entry.status, CustomThemeStatus::Warning { .. }) {
                                    theme.warning
                                } else {
                                    theme.text_muted
                                },
                            )
                            .child(SharedString::from(status)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .when(linked, |actions| {
                        actions.child(
                            compact_action(theme, "Reload", format!("theme-reload-{id}")).on_click(
                                cx.listener({
                                    let id = id.clone();
                                    move |_, _, _, cx| {
                                        let _ = theme_library::reload(&id, cx);
                                        cx.notify();
                                    }
                                }),
                            ),
                        )
                    })
                    .child(
                        compact_action(theme, "Reveal", format!("theme-reveal-{id}")).on_click(
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    if let Err(error) = theme_library::reveal(&id, cx) {
                                        this.library_error = Some(error.to_string().into());
                                    }
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child(
                        compact_action(theme, "Review", format!("theme-review-{id}")).on_click(
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.review_entry = Some(id.clone());
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child(
                        compact_action(
                            theme,
                            "Duplicate as editable",
                            format!("theme-duplicate-{id}"),
                        )
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| {
                                if let Err(error) = theme_library::duplicate_as_editable(&id, cx) {
                                    this.library_error = Some(error.to_string().into());
                                }
                                cx.notify();
                            }
                        })),
                    )
                    .when(linked, |actions| {
                        actions.child(
                            compact_action(theme, "Unlink", format!("theme-unlink-{id}")).on_click(
                                cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        if let Err(error) = theme_library::unlink(&id, cx) {
                                            this.library_error = Some(error.to_string().into());
                                        }
                                        cx.notify();
                                    }
                                }),
                            ),
                        )
                    })
                    .child(
                        compact_action(theme, "Remove", format!("theme-remove-{id}"))
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(error) = theme_library::remove(&id, cx) {
                                    this.library_error = Some(error.to_string().into());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_theme_library_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entries = theme_library::entries(cx);
        let (linked, imported): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| entry.source.is_linked());
        let mut rows = vec![
            widgets::flat_row()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(widgets::row_title(theme, "Theme library"))
                        .child(widgets::row_description(
                            theme,
                            "Import or link custom themes.",
                        )),
                )
                .child(
                    popover::btn_primary(theme, "Add theme")
                        .id("theme-library-add")
                        .on_click(cx.listener(|this, _, _, cx| this.open_import(cx))),
                )
                .into_any_element(),
        ];
        if !imported.is_empty() {
            rows.push(
                div()
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .text_size(crate::typography::ui_rems(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child("IMPORTED")
                    .into_any_element(),
            );
            rows.extend(
                imported
                    .into_iter()
                    .map(|entry| self.render_library_entry(entry, theme, cx)),
            );
        }
        if !linked.is_empty() {
            rows.push(
                div()
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .text_size(crate::typography::ui_rems(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child("LINKED")
                    .into_any_element(),
            );
            rows.extend(
                linked
                    .into_iter()
                    .map(|entry| self.render_library_entry(entry, theme, cx)),
            );
        }
        rows
    }
}
