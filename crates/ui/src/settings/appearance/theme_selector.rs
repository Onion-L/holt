//! Per-appearance theme selection and the accent/surface preference controls.

use super::*;

pub(super) fn accent_helper(accent: AccentSelection) -> String {
    match accent {
        AccentSelection::ThemeDefault => {
            "Theme default · Uses the palette's intended color.".into()
        }
        AccentSelection::Preset(preset) => format!(
            "{} · Controls, glyphs, selections, code, and activity.",
            preset.label()
        ),
    }
}

pub(super) fn surface_label(surface: SurfacePreference) -> &'static str {
    match surface {
        SurfacePreference::ThemeDefault => "Theme default",
        SurfacePreference::Frosted => "Frosted",
        SurfacePreference::Opaque => "Opaque",
    }
}

pub(super) fn surface_helper(surface: SurfacePreference, resolved: SurfaceTreatment) -> String {
    match surface {
        SurfacePreference::ThemeDefault => format!(
            "Uses this theme's {} default.",
            match resolved {
                SurfaceTreatment::Frosted => "frosted",
                SurfaceTreatment::Opaque => "opaque",
            }
        ),
        SurfacePreference::Frosted => "Theme-colored glass where supported.".into(),
        SurfacePreference::Opaque => "Solid surfaces for every theme.".into(),
    }
}

pub(super) fn surface_choice(
    theme: &Theme,
    surface: SurfacePreference,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!(
            "appearance-surface-{}",
            surface_label(surface).to_lowercase().replace(' ', "-")
        )))
        .h(px(30.0))
        .px(px(10.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .bg(if selected {
            theme.accent_wash
        } else {
            theme.surface_raised.opacity(0.28)
        })
        .text_size(crate::typography::ui_rems(11.5))
        .font_weight(if selected {
            gpui::FontWeight::MEDIUM
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.accent
        } else {
            theme.text_muted
        })
        .flex()
        .items_center()
        .cursor_pointer()
        .when(!selected, |control| {
            control.hover(|style| style.bg(theme.surface_raised_hover))
        })
        .child(surface_label(surface))
}

pub(super) fn accent_swatch(
    page_theme: &Theme,
    selection: AccentSelection,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let swatch_theme = Theme::for_selection(
        page_theme.appearance,
        page_theme.variant_id.as_ref(),
        selection,
        page_theme.surface_preference,
    );
    let sample = match selection {
        AccentSelection::ThemeDefault => div()
            .size_full()
            .rounded(px(6.0))
            .bg(swatch_theme.accent_wash)
            .flex()
            .items_center()
            .justify_center()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(4.0))
                    .h(px(13.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.light),
            )
            .child(
                div()
                    .w(px(4.0))
                    .h(px(16.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.mid),
            )
            .child(
                div()
                    .w(px(4.0))
                    .h(px(11.0))
                    .rounded(px(2.0))
                    .bg(swatch_theme.glyph.deep),
            ),
        AccentSelection::Preset(_) => div().size_full().rounded(px(6.0)).bg(swatch_theme.accent),
    };
    div()
        .id(SharedString::from(format!("accent-{}", selection.label())))
        .flex_none()
        .w(px(30.0))
        .h(px(34.0))
        .pb(px(4.0))
        .border_b_2()
        .border_color(if selected {
            swatch_theme.accent
        } else {
            gpui::transparent_black()
        })
        .cursor_pointer()
        .child(
            div()
                .size(px(30.0))
                .p(px(2.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if selected {
                    page_theme.border_strong
                } else {
                    page_theme.border
                })
                .bg(page_theme.surface_raised.opacity(0.42))
                .child(sample),
        )
}

impl AppearancePage {
    pub(super) fn theme_menu(&self, appearance: Appearance) -> &Popup<()> {
        match appearance {
            Appearance::Light => &self.light_theme_menu,
            Appearance::Dark => &self.dark_theme_menu,
        }
    }

    pub(super) fn theme_menu_mut(&mut self, appearance: Appearance) -> &mut Popup<()> {
        match appearance {
            Appearance::Light => &mut self.light_theme_menu,
            Appearance::Dark => &mut self.dark_theme_menu,
        }
    }

    pub(super) fn close_theme_menu(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        if !self.theme_menu_mut(appearance).begin_close() {
            return;
        }
        match appearance {
            Appearance::Light => popover::reap_popup(cx, |page| &mut page.light_theme_menu),
            Appearance::Dark => popover::reap_popup(cx, |page| &mut page.dark_theme_menu),
        }
    }

    pub(super) fn render_theme_selector(
        &mut self,
        appearance_kind: Appearance,
        selections: &ThemeSelection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let registry = ThemeRegistry::active();
        let selected_id = selections
            .variant_id(model_appearance(appearance_kind))
            .to_owned();
        let selected_variant = registry
            .variant(&selected_id)
            .or_else(|| {
                registry
                    .variants_for(model_appearance(appearance_kind))
                    .next()
            })
            .expect("the built-in registry has both appearances");
        let selected_theme = Theme::for_selection(
            appearance_kind,
            &selected_variant.id,
            AccentSelection::ThemeDefault,
            theme.surface_preference,
        );
        let open = self.theme_menu(appearance_kind).is_open();

        let mut trigger = div()
            .id(SharedString::from(format!(
                "{}-theme-selector",
                if appearance_kind.is_light() {
                    "light"
                } else {
                    "dark"
                }
            )))
            .relative()
            .flex_none()
            .w(px(218.0))
            .h(px(34.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_raised.opacity(if open { 0.75 } else { 0.42 }))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .when(!open, |el| {
                el.hover(|style| style.bg(theme.surface_raised_hover))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.theme_menu_mut(appearance_kind).note_trigger_press();
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.theme_menu_mut(appearance_kind).take_press_was_open() {
                    this.close_theme_menu(appearance_kind, cx);
                } else {
                    let other = if appearance_kind.is_light() {
                        Appearance::Dark
                    } else {
                        Appearance::Light
                    };
                    this.close_theme_menu(other, cx);
                    this.theme_menu_mut(appearance_kind).open(());
                }
                cx.notify();
            }))
            .child(palette_preview(&selected_theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(selected_variant.name.clone())),
            )
            .child(
                icons::icon(icons::SORT_VERTICAL)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.45 })),
            );

        if self.theme_menu(appearance_kind).get().is_some() {
            let closing = self.theme_menu(appearance_kind).closing_since();
            let heading = if appearance_kind.is_light() {
                "Light themes"
            } else {
                "Dark themes"
            };
            let menu = popover::popover_card(theme)
                .w(px(260.0))
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    this.close_theme_menu(appearance_kind, cx);
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, heading))
                .children(
                    registry
                        .variants_for(model_appearance(appearance_kind))
                        .enumerate()
                        .map(|(index, variant)| {
                            let id = variant.id.clone();
                            let name = variant.name.clone();
                            let active = id == selected_id;
                            let sample = Theme::for_selection(
                                appearance_kind,
                                &id,
                                AccentSelection::ThemeDefault,
                                theme.surface_preference,
                            );
                            popover::menu_row(
                                theme,
                                active,
                                SharedString::from(format!(
                                    "appearance-theme-menu-{appearance_kind:?}-{index}"
                                )),
                            )
                            .id(SharedString::from(format!(
                                "appearance-theme-row-{appearance_kind:?}-{index}"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                appearance::set_theme(appearance_kind, id.clone(), cx);
                                this.close_theme_menu(appearance_kind, cx);
                                cx.notify();
                            }))
                            .child(palette_preview(&sample))
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .when(active, |row| {
                                row.child(
                                    icons::icon(icons::CHECK)
                                        .size(px(14.0))
                                        .text_color(theme.accent),
                                )
                            })
                        }),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_below(
                SharedString::from(format!(
                    "appearance-{}-theme-menu",
                    if appearance_kind.is_light() {
                        "light"
                    } else {
                        "dark"
                    }
                )),
                menu,
                closing,
            ));
        }

        trigger.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_helper_explains_default_and_override_scope() {
        assert!(accent_helper(AccentSelection::ThemeDefault).contains("intended"));
        let copy = accent_helper(AccentSelection::Preset(AccentPreset::Pink));
        assert!(copy.starts_with("Pink ·"));
        assert!(copy.contains("glyphs"));
    }

    #[test]
    fn surface_helper_explains_theme_default_and_global_overrides() {
        let default = surface_helper(SurfacePreference::ThemeDefault, SurfaceTreatment::Opaque);
        assert!(default.contains("opaque default"));
        assert!(
            surface_helper(SurfacePreference::Frosted, SurfaceTreatment::Opaque)
                .contains("where supported")
        );
        assert!(
            surface_helper(SurfacePreference::Opaque, SurfaceTreatment::Frosted)
                .contains("every theme")
        );
    }
}
