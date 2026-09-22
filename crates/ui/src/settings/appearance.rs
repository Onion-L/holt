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
    /// Chat backdrop path field — rendered only in path-editing mode (the
    /// quiet filename row swaps for it); edits persist debounced (see
    /// [`Self::save_backdrop`]).
    backdrop_input: Entity<ComposerInput>,
    /// Whether the path field is showing instead of the filename row.
    backdrop_editing: bool,
    /// The field's current text is not an existing file — the stored path is
    /// left untouched until it resolves (or the field clears).
    backdrop_error: Option<SharedString>,
    _backdrop_events: Subscription,
    backdrop_focus_subs: Vec<Subscription>,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let backdrop_input = cx.new(|cx| ComposerInput::new("Path to an image — ~/… works", cx));
        if let Some(path) = crate::settings::chat_backdrop(cx).0 {
            backdrop_input.update(cx, |input, cx| input.set_text(path, cx));
        }
        let _backdrop_events = cx.subscribe(
            &backdrop_input,
            |page: &mut Self, _, event: &ComposerInputEvent, cx: &mut Context<Self>| {
                match event {
                    ComposerInputEvent::Edited => page.save_backdrop(cx),
                    // Enter commits (the saves are continuous anyway) and
                    // leaves edit mode; the render pass drops the stranded
                    // focus.
                    ComposerInputEvent::Submitted => {
                        page.backdrop_editing = false;
                        cx.notify();
                    }
                    _ => {}
                }
            },
        );
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
            backdrop_input,
            backdrop_editing: false,
            backdrop_error: None,
            _backdrop_events,
            backdrop_focus_subs: Vec::new(),
        }
    }

    /// Persist the field's text: empty clears the backdrop, an existing file
    /// replaces it, and anything else is refused with a hint (a stored path
    /// always loads). Fires on every keystroke via the Edited subscription;
    /// the write itself is debounced. Only an actual change repaints other
    /// windows — a validity wiggle keeps the pane as-is.
    fn save_backdrop(&mut self, cx: &mut Context<Self>) {
        let raw = self.backdrop_input.read(cx).text().trim().to_owned();
        self.backdrop_error = None;
        let changed = if raw.is_empty() {
            crate::settings::update(crate::settings::SavePolicy::Debounced, cx, |settings| {
                settings.chat_backdrop_path = None;
            })
        } else if crate::chat_backdrop::expand_path(&raw).is_file() {
            crate::settings::update(crate::settings::SavePolicy::Debounced, cx, |settings| {
                settings.chat_backdrop_path = Some(raw);
            })
        } else {
            self.backdrop_error = Some(SharedString::from(
                "That file does not exist — fix the path (or clear the field) to apply.",
            ));
            false
        };
        if changed {
            cx.refresh_windows();
        }
        cx.notify();
    }

    /// Point the field at `path` — the native picker's choice or a dropped
    /// file — and persist it through [`Self::save_backdrop`].
    fn apply_backdrop_path(&mut self, path: String, cx: &mut Context<Self>) {
        self.backdrop_input
            .update(cx, |input, cx| input.set_text(path, cx));
        self.save_backdrop(cx);
    }

    /// Enter path-editing mode: the filename row swaps for the field,
    /// pre-filled with the stored path.
    fn start_backdrop_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = crate::settings::chat_backdrop(cx).0.unwrap_or_default();
        self.backdrop_input
            .update(cx, |input, cx| input.set_text(stored, cx));
        self.backdrop_error = None;
        self.backdrop_editing = true;
        let focus = self.backdrop_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Leave path-editing mode on blur: the field is a staging area, the
    /// stored path is what the pane renders — reset to it and discard any
    /// validation hint with the edit.
    fn finish_backdrop_edit(&mut self, cx: &mut Context<Self>) {
        self.backdrop_editing = false;
        self.backdrop_error = None;
        let stored = crate::settings::chat_backdrop(cx).0.unwrap_or_default();
        self.backdrop_input
            .update(cx, |input, cx| input.set_text(stored, cx));
        cx.notify();
    }

    /// Open the native image picker and apply the choice. The picker only
    /// exists on macOS; other platforms keep the field as the entry point.
    fn browse_for_backdrop(cx: &mut Context<Self>) {
        // `runModal` pumps AppKit's nested event loop: it must run OUTSIDE
        // every gpui borrow, or the loop's re-entrant callbacks (mouse-move,
        // draw) collide with the window's held RefCell (crash: "RefCell
        // already borrowed"). Spawn onto the foreground queue — the body
        // runs after this dispatch fully unwinds, on the main thread, with
        // no borrows held.
        cx.spawn(async move |page, cx| {
            let Some(picked) = crate::file_dialog::pick_image() else {
                return;
            };
            let picked = picked.display().to_string();
            page.update(cx, |page, cx| page.apply_backdrop_path(picked, cx))
                .ok();
        })
        .detach();
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

        // Chat backdrop: the preview is the setting — a full-width live
        // render of the same wash recipe the pane runs, doubling as the
        // drop target and (on macOS) click-to-replace. The quiet row under
        // it carries the file's identity (click to edit the path) and the
        // presence segments. The preview shares the pane's session cache:
        // once the chat has loaded the picture, the preview is free.
        let (backdrop_path, backdrop_presence) = crate::settings::chat_backdrop(cx);
        let configured = backdrop_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let dark = !theme.appearance.is_light();
        // The preview shows the frosted variant — what the column reads like
        // once the conversation is underway.
        let backdrop_blur = crate::chat_backdrop::FROST_SIGMA;
        if let Some(path) = configured
            .filter(|path| crate::chat_backdrop::cached(path, dark, backdrop_blur).is_none())
        {
            crate::chat_backdrop::preload(path, dark, backdrop_blur, cx);
        }
        let loaded =
            configured.and_then(|path| crate::chat_backdrop::cached(path, dark, backdrop_blur));
        let decode_failed =
            configured.is_some_and(|path| crate::chat_backdrop::failed(path, dark, backdrop_blur));
        let loading = configured.is_some() && loaded.is_none() && !decode_failed;
        // Path-editing focus: clicking away commits (blur), and Enter hides
        // the field — drop its stranded focus so keystrokes don't fall into
        // an unmounted input.
        let backdrop_focus = self.backdrop_input.read(cx).focus_handle(cx);
        if self.backdrop_focus_subs.is_empty() {
            self.backdrop_focus_subs
                .push(cx.on_blur(&backdrop_focus, window, |page, _, cx| {
                    page.finish_backdrop_edit(cx);
                }));
        }
        if !self.backdrop_editing && backdrop_focus.is_focused(window) {
            window.blur();
        }
        let preview_message = |theme: &Theme| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_faint)
        };
        let backdrop_preview = div()
            .id("backdrop-preview")
            .relative()
            .w_full()
            .h(px(184.0))
            .rounded(px(8.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_raised.opacity(0.5))
            .cursor_pointer()
            .hover(|style| style.border_color(theme.border_strong))
            // OS file drops land here (gpui turns them into an
            // `ExternalPaths` drag); hover styles are suspended mid-drag, so
            // the accent ring always wins.
            .drag_over::<gpui::ExternalPaths>(|style, _, _, cx| {
                let theme = Theme::of(cx);
                style.border_color(theme.accent).bg(theme.accent.opacity(0.08))
            })
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |page, paths: &gpui::ExternalPaths, _, cx| {
                    let picked = paths
                        .paths()
                        .iter()
                        .find(|path| crate::chat_backdrop::is_supported_file(path));
                    match picked {
                        Some(path) => page.apply_backdrop_path(path.display().to_string(), cx),
                        None => {
                            page.backdrop_error = Some(SharedString::from(
                                "That drop is not an image Holt can decode — png, jpeg, webp, or gif works.",
                            ));
                            cx.notify();
                        }
                    }
                },
            ))
            .on_click(cx.listener(|page, _, window, cx| {
                if cfg!(target_os = "macos") {
                    Self::browse_for_backdrop(cx);
                } else {
                    page.start_backdrop_edit(window, cx);
                }
            }))
            .when_some(loaded, |card, loaded| {
                let wash = crate::chat_backdrop::wash(
                    backdrop_presence,
                    loaded.luminance,
                    theme.surface.l,
                );
                // Sunk placeholder bars: what transcript rows read like at
                // the bottom of the wash.
                let bars = [96.0, 64.0, 80.0].into_iter().map(|width| {
                    div()
                        .w(px(width))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(theme.text.opacity(0.16))
                });
                card.child(crate::chat_backdrop::paint_layers(
                    loaded.image.clone(),
                    theme.surface,
                    wash,
                ))
                .child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_3()
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .children(bars),
                )
            })
            .when(loading, |card| card.child(preview_message(&theme).child("Loading…")))
            .when(decode_failed, |card| {
                card.border_color(theme.warning.opacity(0.4)).child(
                    preview_message(&theme)
                        .gap(px(6.0))
                        .child(
                            icons::icon(icons::DANGER_TRIANGLE)
                                .size(px(16.0))
                                .text_color(theme.warning_muted),
                        )
                        .child("Not a decodable image"),
                )
            })
            .when(configured.is_none(), |card| {
                card.border_dashed().child(
                    preview_message(&theme)
                        .child(div().text_color(theme.text_muted).child("Drop an image here"))
                        .child(if cfg!(target_os = "macos") {
                            "or click to browse"
                        } else {
                            "or paste a path"
                        }),
                )
            });
        let presence_control = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_raised.opacity(0.34))
            .children(
                [("Subtle", 0.25_f32), ("Balanced", 0.5), ("Bold", 0.75)]
                    .into_iter()
                    .map(|(label, value)| {
                        let selected = (backdrop_presence - value).abs() < 0.01;
                        div()
                            .id(SharedString::from(format!("backdrop-presence-{label}")))
                            .px(px(10.0))
                            .py(px(3.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
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
                            .when(selected, |segment| segment.bg(theme.accent.opacity(0.16)))
                            .when(!selected, |segment| {
                                segment.hover(|style| {
                                    style.bg(theme.element_hover).text_color(theme.text)
                                })
                            })
                            .child(label)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                crate::settings::update(
                                    crate::settings::SavePolicy::Immediate,
                                    cx,
                                    |settings| {
                                        settings.chat_backdrop_presence = value;
                                    },
                                );
                                cx.refresh_windows();
                                cx.notify();
                            }))
                    }),
            );
        let backdrop_issue = self.backdrop_error.clone().or_else(|| {
            decode_failed.then(|| {
                SharedString::from(
                    "The file exists, but Holt could not decode it as an image — png, jpeg, webp, or gif works.",
                )
            })
        });
        // The quiet row under the preview: the file's identity (click to
        // edit the path — Enter or clicking away commits) and the presence
        // segments, whose effect the preview shows live.
        let backdrop_path_row: AnyElement = if self.backdrop_editing {
            div()
                .flex_1()
                .min_w_0()
                .px(px(10.0))
                .py(px(5.0))
                .rounded(px(Theme::CONTROL_RADIUS))
                .border_1()
                .border_color(theme.border)
                .bg(theme.input_glass_bg())
                .child(self.backdrop_input.clone())
                .into_any_element()
        } else if let Some(path) = configured {
            let name = crate::chat_backdrop::expand_path(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| path.to_owned());
            let full_path: SharedString = path.to_owned().into();
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .child(
                    div()
                        .id("backdrop-path-edit")
                        .min_w_0()
                        .truncate()
                        .rounded(px(6.0))
                        .px(px(6.0))
                        .py(px(3.0))
                        .cursor_pointer()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .hover(|style| style.bg(theme.element_hover).text_color(theme.text))
                        .tooltip(move |_, cx| {
                            cx.new(|_| crate::image_viewer::ViewerTooltip(full_path.clone()))
                                .into()
                        })
                        .child(SharedString::from(name))
                        .on_click(
                            cx.listener(|page, _, window, cx| page.start_backdrop_edit(window, cx)),
                        ),
                )
                .child(
                    div()
                        .id("backdrop-clear")
                        .flex_none()
                        .rounded(px(6.0))
                        .p(px(4.0))
                        .cursor_pointer()
                        .child(
                            icons::icon(icons::CLOSE)
                                .size(px(11.0))
                                .text_color(theme.text_faint),
                        )
                        .hover(|style| style.bg(theme.element_hover))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.apply_backdrop_path(String::new(), cx)
                        })),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .child(
                    div()
                        .id("backdrop-path-edit")
                        .rounded(px(6.0))
                        .px(px(6.0))
                        .py(px(3.0))
                        .cursor_pointer()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_faint)
                        .hover(|style| style.bg(theme.element_hover).text_color(theme.text_muted))
                        .child("Enter a path…")
                        .on_click(
                            cx.listener(|page, _, window, cx| page.start_backdrop_edit(window, cx)),
                        ),
                )
                .into_any_element()
        };
        let backdrop_card = div()
            .p(px(12.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(backdrop_preview)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(backdrop_path_row)
                    .child(presence_control),
            )
            .when_some(backdrop_issue, |card, issue| {
                card.child(widgets::warning_strip(&theme, issue))
            });
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
                            .mt(px(20.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Chat backdrop"))
                            .child(backdrop_card),
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
