//! Settings → Appearance: the page assembling the mode switch, theme
//! selectors, accent/surface controls, theme library, and font pickers.
//!
//! `fonts` — interface font/size pickers and keyboard navigation.
//! `previews` — theme preview miniatures.
//! `theme_selector` — per-appearance theme menus, accent and surface.
//! `import` — the VS Code theme import flow and modal.
//! `library` — custom-theme library rows and the review dialog.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use holt_theme::vscode::{ImportReport, SourceCompilation};
use holt_theme::{
    AccentPreset, AccentSelection, CustomThemeEntry, CustomThemeStatus, InstallMode,
    SurfacePreference, SurfaceTreatment, ThemeRegistry, ThemeSelection,
};

use crate::appearance::{self, AppearanceMode};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::popover::{self, Popup};
use crate::settings::widgets;
use crate::theme::{Appearance, Theme};
use crate::theme_library;
use crate::typography::{self, FontAvailability, UiFontFamily, UiFontSize};

mod fonts;
mod import;
mod library;
mod previews;
mod theme_selector;

use import::ImportDialog;
use previews::*;
use theme_selector::*;

pub struct AppearancePage {
    selected_font: UiFontFamily,
    selected_size: UiFontSize,
    font_focus: FocusHandle,
    size_focus: FocusHandle,
    font_menu: Popup<()>,
    size_menu: Popup<()>,
    font_menu_dismissed_at: Option<std::time::Instant>,
    size_menu_dismissed_at: Option<std::time::Instant>,
    light_theme_menu: Popup<()>,
    dark_theme_menu: Popup<()>,
    import_dialog: Option<ImportDialog>,
    review_entry: Option<String>,
    library_error: Option<SharedString>,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            selected_font: typography::effective(cx),
            selected_size: typography::font_size(cx),
            font_focus: cx.focus_handle(),
            size_focus: cx.focus_handle(),
            font_menu: Popup::default(),
            size_menu: Popup::default(),
            font_menu_dismissed_at: None,
            size_menu_dismissed_at: None,
            light_theme_menu: Popup::default(),
            dark_theme_menu: Popup::default(),
            import_dialog: None,
            review_entry: None,
            library_error: None,
        }
    }
}

fn model_appearance(appearance: Appearance) -> holt_theme::Appearance {
    match appearance {
        Appearance::Dark => holt_theme::Appearance::Dark,
        Appearance::Light => holt_theme::Appearance::Light,
    }
}

fn compact_action(
    theme: &Theme,
    label: &str,
    id: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    popover::btn_ghost(theme, label, id.clone())
        .id(id)
        .h(px(28.0))
        .px(px(9.0))
        .py(px(0.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_raised.opacity(0.34))
        .flex()
        .items_center()
        .text_size(crate::typography::ui_rems(11.5))
}

impl Render for AppearancePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let effective_font = typography::effective(cx);
        let requested_font = typography::requested(cx);
        let availability = typography::availability(cx);
        let fixed = theme.font_sans_fixed.clone();
        let current_mode = appearance::mode(cx);
        let current_themes = appearance::themes(cx);
        let current_accent = appearance::accent(cx);
        let current_surface = appearance::surface(cx);
        // Mode switch: one preview card per appearance — each paints a
        // miniature of the app in that theme (System splits light/dark down
        // the middle), and the selected card carries the accent border.
        let preview_themes = |appearance_kind: Appearance| {
            Theme::for_selection(
                appearance_kind,
                current_themes.variant_id(model_appearance(appearance_kind)),
                current_accent,
                theme.surface_preference,
            )
        };
        let light_preview = preview_themes(Appearance::Light);
        let dark_preview = preview_themes(Appearance::Dark);
        let mode_switch = div().w_full().flex().flex_row().gap(px(10.0)).children(
            AppearanceMode::ALL.into_iter().map(|mode| {
                let selected = mode == current_mode;
                div()
                    .id(SharedString::from(format!("appearance-{}", mode.label())))
                    .flex_1()
                    .min_w_0()
                    .p(px(5.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .when(!selected, |card| {
                        card.hover(|style| style.border_color(theme.border_strong))
                    })
                    .child(
                        div()
                            .h(px(124.0))
                            .rounded(px(8.0))
                            .overflow_hidden()
                            .child(mode_preview(mode, &light_preview, &dark_preview)),
                    )
                    .child(
                        div()
                            .w_full()
                            .pb(px(2.0))
                            .text_center()
                            .text_size(crate::typography::ui_rems(12.0))
                            .font_weight(if selected {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(mode.label()),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        appearance::set_mode(mode, cx);
                        cx.notify();
                    }))
            }),
        );

        let mut theme_rows = Vec::new();
        for appearance_kind in [Appearance::Light, Appearance::Dark] {
            let label = if appearance_kind.is_light() {
                "Light theme"
            } else {
                "Dark theme"
            };
            let selector = self.render_theme_selector(appearance_kind, &current_themes, &theme, cx);
            theme_rows.push(
                widgets::flat_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(widgets::row_title(&theme, label))
                            .child(widgets::row_description(
                                &theme,
                                "Used whenever this appearance is active.",
                            )),
                    )
                    .child(div().flex_none().child(selector))
                    .into_any_element(),
            );
        }

        let mut accent_choices = vec![AccentSelection::ThemeDefault];
        accent_choices.extend(AccentPreset::ALL.map(AccentSelection::Preset));
        let accent_controls = accent_choices
            .into_iter()
            .map(|selection| {
                let selected = selection == current_accent;
                accent_swatch(&theme, selection, selected).on_click(cx.listener(
                    move |_, _, _, cx| {
                        appearance::set_accent(selection, cx);
                        cx.notify();
                    },
                ))
            })
            .collect::<Vec<_>>();
        let surface_controls = SurfacePreference::ALL
            .into_iter()
            .map(|surface| {
                surface_choice(&theme, surface, surface == current_surface).on_click(cx.listener(
                    move |_, _, _, cx| {
                        appearance::set_surface(surface, cx);
                        cx.notify();
                    },
                ))
            })
            .collect::<Vec<_>>();
        let mut settings_rows = theme_rows;
        settings_rows.push(
            widgets::flat_row()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(widgets::row_title(&theme, "Accent color"))
                        .child(widgets::row_description(
                            &theme,
                            accent_helper(current_accent),
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(accent_controls),
                )
                .into_any_element(),
        );
        settings_rows.push(
            widgets::flat_row()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(widgets::row_title(&theme, "Glass"))
                        .child(widgets::row_description(
                            &theme,
                            surface_helper(current_surface, theme.surface_treatment),
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(surface_controls),
                )
                .into_any_element(),
        );
        settings_rows.extend(self.render_theme_library_rows(&theme, cx));
        let library_warning = self
            .library_error
            .clone()
            .or_else(|| theme_library::load_warning(cx).map(SharedString::from));
        let modal = self
            .render_import_dialog(window.viewport_size(), &theme, window, cx)
            .or_else(|| self.render_review_dialog(window.viewport_size(), &theme, cx));

        let (font_trigger, size_trigger) =
            self.render_font_controls(&theme, &availability, &effective_font, &fixed, cx);

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Appearance", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose how Holt looks. These settings stay on this device.",
                        )
                        .max_w(px(640.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Appearance"))
                            .child(mode_switch),
                    )
                    .child(
                        div()
                            .mt(px(20.0))
                            .flex()
                            .flex_col()
                            .children(settings_rows),
                    )
                    .child(
                        div()
                            .mt(px(36.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .font_family(fixed.clone())
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(24.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.0))
                                            .child(widgets::field_label(&theme, "Interface font"))
                                            .child(
                                                div()
                                                    .max_w(px(640.0))
                                                    .text_size(typography::ui_rems(12.0))
                                                    .line_height(px(18.0))
                                                    .text_color(theme.text_muted)
                                                    .child(SharedString::from(
                                                        "Used across the interface and conversations. Code, diffs, and terminal keep their current fonts and sizes.",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(font_trigger)
                                            .child(size_trigger),
                                    ),
                            )
                            .when(requested_font != effective_font, |section| {
                                section.child(
                                    widgets::error_strip(
                                        &theme,
                                        "This font could not be loaded. Holt is using Geist.",
                                    )
                                    .font_family(fixed.clone()),
                                )
                            }),
                    )
                    .when_some(library_warning, |page, warning| {
                        page.child(
                            div()
                                .mt(px(8.0))
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.warning)
                                .child(warning),
                        )
                    }),
            )
            .children(modal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_segment() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn registry_offers_both_appearances_and_keeps_single_dark_families_valid() {
        let registry = ThemeRegistry::builtin();
        assert_eq!(
            registry.variants_for(holt_theme::Appearance::Light).count(),
            10
        );
        assert_eq!(
            registry.variants_for(holt_theme::Appearance::Dark).count(),
            20
        );
    }
}
