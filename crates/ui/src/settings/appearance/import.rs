//! The VS Code theme import flow: source picking, compilation, the variant
//! list, and the import modal.

use super::*;

pub(super) struct ImportDialog {
    input: Entity<ComposerInput>,
    _events: Subscription,
    focus: FocusHandle,
    focus_pending: bool,
    mode: InstallMode,
    compilation: Option<SourceCompilation>,
    selected: HashSet<String>,
    review_variant: Option<String>,
    error: Option<SharedString>,
}

impl AppearancePage {
    pub(super) fn open_import(&mut self, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            ComposerInput::with_context(
                "Theme file, package.json, or extension folder",
                "PaletteSearch",
                cx,
            )
        });
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                let source = this
                    .import_dialog
                    .as_ref()
                    .map(|dialog| PathBuf::from(dialog.input.read(cx).text().trim()));
                if let Some(dialog) = this.import_dialog.as_mut()
                    && dialog
                        .compilation
                        .as_ref()
                        .zip(source.as_ref())
                        .is_some_and(|(compilation, source)| compilation.path != *source)
                {
                    dialog.compilation = None;
                    dialog.selected.clear();
                    dialog.review_variant = None;
                    dialog.error = None;
                    cx.notify();
                }
            }
            ComposerInputEvent::Submitted => {
                if this
                    .import_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.compilation.is_some())
                {
                    this.finish_import(cx);
                } else {
                    this.compile_import(cx);
                }
            }
            _ => {}
        });
        self.import_dialog = Some(ImportDialog {
            input,
            _events: events,
            focus: cx.focus_handle(),
            focus_pending: true,
            mode: InstallMode::Snapshot,
            compilation: None,
            selected: HashSet::new(),
            review_variant: None,
            error: None,
        });
        cx.notify();
    }

    pub(super) fn compile_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        let source = dialog.input.read(cx).text().trim().to_owned();
        if source.is_empty() {
            dialog.error = Some("Choose a local theme file or extension folder.".into());
            cx.notify();
            return;
        }
        let path = PathBuf::from(&source);
        let family_name = source_name(&path);
        let family_id = format!("custom-{}", slug(&family_name));
        match theme_library::compile(&path, &family_id, &family_name) {
            Ok(compilation) => {
                dialog.selected = compilation
                    .family
                    .variants
                    .iter()
                    .map(|variant| variant.id.clone())
                    .collect();
                // Mapping diagnostics are useful, but they are an advanced
                // inspection surface rather than part of the happy path.
                dialog.review_variant = None;
                dialog.compilation = Some(compilation);
                dialog.error = None;
            }
            Err(error) => dialog.error = Some(error.to_string().into()),
        }
        cx.notify();
    }

    pub(super) fn choose_import_source(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Choose Theme Source".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            let _ = this.update(cx, |page, cx| {
                if let Some(dialog) = page.import_dialog.as_mut() {
                    dialog.input.update(cx, |input, cx| {
                        input.set_text(path.display().to_string(), cx)
                    });
                }
                page.compile_import(cx);
            });
        })
        .detach();
    }

    pub(super) fn finish_import(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog.as_mut() else {
            return;
        };
        if dialog.selected.is_empty() {
            dialog.error = Some("Select at least one variant to import.".into());
            cx.notify();
            return;
        }
        let Some(compilation) = dialog.compilation.take() else {
            return;
        };
        let selected = dialog.selected.iter().cloned().collect::<Vec<_>>();
        match theme_library::install(compilation.clone(), &selected, dialog.mode, cx) {
            Ok(_) => self.import_dialog = None,
            Err(error) => {
                dialog.compilation = Some(compilation);
                dialog.error = Some(error.to_string().into());
            }
        }
        cx.notify();
    }

    pub(super) fn render_import_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        {
            let dialog = self.import_dialog.as_mut()?;
            if std::mem::take(&mut dialog.focus_pending) {
                let input_focus = dialog.input.focus_handle(cx);
                window.focus(&input_focus, cx);
            }
        }
        let dialog = self.import_dialog.as_ref()?;
        let input = dialog.input.clone();
        let focus = dialog.focus.clone();
        let mode = dialog.mode;
        let compilation = dialog.compilation.clone();
        let selected = dialog.selected.clone();
        let review_variant = dialog.review_variant.clone();
        let error = dialog.error.clone();
        let ready = compilation.is_some() && !selected.is_empty();
        let hairline = crate::theme::hairline(0.08);

        let mode_control = |label: &'static str, description: &'static str, value: InstallMode| {
            let active = mode == value;
            div()
                .id(SharedString::from(format!(
                    "theme-import-mode-{}",
                    slug(label)
                )))
                .flex_1()
                .min_w_0()
                .p(px(10.0))
                .rounded(px(9.0))
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .bg(if active {
                    theme.accent_wash
                } else {
                    theme.surface_raised.opacity(0.28)
                })
                .cursor_pointer()
                .when(!active, |control| {
                    control.hover(|style| style.bg(theme.surface_raised_hover))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(dialog) = this.import_dialog.as_mut() {
                        dialog.mode = value;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(
                            div()
                                .size(px(16.0))
                                .rounded_full()
                                .border_1()
                                .border_color(if active {
                                    theme.accent
                                } else {
                                    theme.border_strong
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(active, |dot| {
                                    dot.child(div().size(px(8.0)).rounded_full().bg(theme.accent))
                                }),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if active { theme.text } else { theme.text_muted })
                                .child(label),
                        ),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .ml(px(23.0))
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.68))
                        .child(description),
                )
        };

        let section_label = |label: &'static str| {
            div()
                .mb(px(7.0))
                .text_size(crate::typography::ui_rems(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_muted)
                .child(label)
        };

        let mut main = div()
            .id("theme-import-main")
            .max_h(px(520.0))
            .overflow_y_scroll()
            .px(px(20.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .child(section_label("Source"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        popover::dialog_field(input.into_any_element())
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .py(px(0.0))
                            .flex()
                            .items_center(),
                    )
                    .child(
                        compact_action(theme, "Browse…", "theme-import-browse")
                            .h(px(36.0))
                            .px(px(12.0))
                            .flex_none()
                            .on_click(cx.listener(|this, _, _, cx| this.choose_import_source(cx))),
                    ),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .child(section_label("Keep it up to date"))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(mode_control(
                                "Import a copy",
                                "Works independently from the original file.",
                                InstallMode::Snapshot,
                            ))
                            .child(mode_control(
                                "Link to source",
                                "Reload changes from the file on disk.",
                                InstallMode::Link,
                            )),
                    ),
            );

        if let Some(ref compilation) = compilation {
            main = main.child(
                div()
                    .mt(px(18.0))
                    .pt(px(16.0))
                    .border_t_1()
                    .border_color(hairline)
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .child(section_label("Detected themes").mb(px(0.0)))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(format!(
                                "{} variant{}",
                                compilation.family.variants.len(),
                                if compilation.family.variants.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            ))),
                    ),
            );
            for variant in &compilation.family.variants {
                let variant_id = variant.id.clone();
                let selected_now = selected.contains(&variant_id);
                let review_open = review_variant.as_deref() == Some(variant_id.as_str());
                let appearance = if variant.appearance.is_dark() {
                    "Dark"
                } else {
                    "Light"
                };
                let report = compilation.reports.get(&variant.id);
                let sample = Theme::from_variant(
                    variant,
                    AccentSelection::ThemeDefault,
                    SurfacePreference::ThemeDefault,
                );
                main = main.child(
                    div()
                        .id(SharedString::from(format!("theme-import-row-{variant_id}")))
                        .mt(px(8.0))
                        .p(px(11.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(if selected_now {
                            theme.accent.opacity(0.7)
                        } else {
                            theme.border
                        })
                        .bg(if selected_now {
                            theme.accent_wash.opacity(0.42)
                        } else {
                            theme.surface_raised.opacity(0.22)
                        })
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(9.0))
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "theme-import-select-{variant_id}"
                                        )))
                                        .size(px(18.0))
                                        .rounded(px(5.0))
                                        .border_1()
                                        .border_color(if selected_now {
                                            theme.accent
                                        } else {
                                            theme.border_strong
                                        })
                                        .bg(if selected_now { theme.accent } else { theme.bg })
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .when(selected_now, |item| {
                                            item.child(
                                                icons::icon(icons::CHECK)
                                                    .size(px(12.0))
                                                    .text_color(theme.on_accent),
                                            )
                                        })
                                        .on_click(cx.listener({
                                            let variant_id = variant_id.clone();
                                            move |this, _, _, cx| {
                                                if let Some(dialog) = this.import_dialog.as_mut()
                                                    && !dialog.selected.remove(&variant_id)
                                                {
                                                    dialog.selected.insert(variant_id.clone());
                                                }
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(palette_preview(&sample))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(12.5))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child(SharedString::from(variant.name.clone())),
                                        )
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.65))
                                                .child(appearance),
                                        ),
                                )
                                .child(
                                    compact_action(
                                        theme,
                                        if review_open {
                                            "Hide details"
                                        } else {
                                            "Details"
                                        },
                                        format!("theme-import-review-{variant_id}"),
                                    )
                                    .on_click(cx.listener({
                                        let variant_id = variant_id.clone();
                                        move |this, _, _, cx| {
                                            if let Some(dialog) = this.import_dialog.as_mut() {
                                                dialog.review_variant =
                                                    if dialog.review_variant.as_deref()
                                                        == Some(variant_id.as_str())
                                                    {
                                                        None
                                                    } else {
                                                        Some(variant_id.clone())
                                                    };
                                            }
                                            cx.notify();
                                        }
                                    })),
                                ),
                        )
                        .when(review_open, |row| {
                            row.child(
                                div()
                                    .mt(px(10.0))
                                    .pt(px(10.0))
                                    .border_t_1()
                                    .border_color(hairline)
                                    .child(import_scene_preview(variant)),
                            )
                            .when_some(report, |row, report| row.child(report_panel(theme, report)))
                        }),
                );
            }
            for failure in &compilation.failures {
                main = main.child(
                    div()
                        .mt(px(8.0))
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .bg(theme.warning.opacity(0.08))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(format!(
                            "{} could not be compiled · {}",
                            failure.name, failure.message
                        ))),
                );
            }
        } else {
            main = main.child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .items_start()
                    .gap(px(7.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.text_muted.opacity(0.72))
                    .child(
                        icons::icon(icons::INFO_CIRCLE)
                            .size(px(13.0))
                            .mt(px(1.0))
                            .flex_none(),
                    )
                    .child("Holt finds light and dark variants automatically."),
            );
        }

        if let Some(error) = error {
            main = main.child(
                div()
                    .mt(px(12.0))
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.danger.opacity(0.08))
                    .flex()
                    .items_start()
                    .gap(px(7.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.danger)
                    .child(
                        icons::icon(icons::DANGER_TRIANGLE)
                            .size(px(13.0))
                            .flex_none()
                            .mt(px(1.0)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(error)),
            );
        }

        let header = div()
            .px(px(20.0))
            .pt(px(18.0))
            .pb(px(16.0))
            .flex()
            .items_start()
            .gap(px(16.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(popover::dialog_title(theme, "Add a theme"))
                    .child(
                        popover::dialog_body(
                            theme,
                            "Import a local theme into your library or keep it linked to its source.",
                        )
                        .mt(px(4.0)),
                    ),
            )
            .child(
                div()
                    .id("theme-import-close")
                    .size(px(28.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface_raised.opacity(0.28))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.surface_raised_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_dialog = None;
                        cx.notify();
                    }))
                    .child(
                        icons::icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            );

        let footer = div()
            .border_t_1()
            .border_color(hairline)
            .bg(theme.surface_raised.opacity(0.18))
            .px(px(20.0))
            .py(px(12.0))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .child(
                compact_action(theme, "Cancel", "theme-import-cancel")
                    .h(px(34.0))
                    .px(px(13.0))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_dialog = None;
                        cx.notify();
                    })),
            )
            .child(
                popover::btn_primary(
                    theme,
                    if compilation.is_some() {
                        "Import selected"
                    } else {
                        "Analyze theme"
                    },
                )
                .id("theme-import-action")
                .h(px(34.0))
                .px(px(14.0))
                .py(px(0.0))
                .flex()
                .items_center()
                .when(compilation.is_some() && !ready, |button| {
                    button.opacity(0.45)
                })
                .when(compilation.is_none() || ready, |button| {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        if this
                            .import_dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.compilation.is_some())
                        {
                            this.finish_import(cx);
                        } else {
                            this.compile_import(cx);
                        }
                    }))
                }),
            );

        let card = popover::dialog_card(theme)
            .id("theme-import-card")
            .w(px(600.0))
            .max_h(px(760.0))
            .p(px(0.0))
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                match popover::classify_key(
                    event.keystroke.key.as_str(),
                    event.keystroke.modifiers.platform,
                    event.keystroke.modifiers.control,
                ) {
                    popover::MenuKey::Escape => {
                        this.import_dialog = None;
                        cx.notify();
                    }
                    popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                        if this
                            .import_dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.compilation.is_some())
                        {
                            this.finish_import(cx);
                        } else {
                            this.compile_import(cx);
                        }
                    }
                    _ => {}
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.import_dialog = None;
                cx.notify();
            }))
            .child(header)
            .child(main)
            .child(footer)
            .into_any_element();

        Some(popover::modal("theme-import-dialog", viewport, card))
    }
}

fn source_name(path: &Path) -> String {
    let path = if path.file_name().and_then(|name| name.to_str()) == Some("package.json") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Custom theme")
        .to_owned()
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if result.is_empty() {
        "theme".into()
    } else {
        result
    }
}

pub(super) fn import_scene_preview(variant: &holt_theme::ThemeVariant) -> AnyElement {
    let theme = Theme::from_variant(
        variant,
        AccentSelection::ThemeDefault,
        SurfacePreference::ThemeDefault,
    );
    div()
        .w_full()
        .h(px(86.0))
        .flex()
        .gap(px(8.0))
        .child(
            div()
                .w(px(152.0))
                .h_full()
                .overflow_hidden()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .child(miniature(&theme)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .p(px(9.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_family(theme.font_mono.clone())
                        .child(
                            div()
                                .text_color(theme.syntax.keyword)
                                .child("fn ")
                                .child(div().text_color(theme.syntax.function).child("preview"))
                                .child(div().text_color(theme.syntax.punctuation).child("() {")),
                        ),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_family(theme.font_mono.clone())
                        .text_color(theme.syntax.string)
                        .child("  \"Theme mapping\""),
                )
                .child(
                    div()
                        .mt_auto()
                        .h(px(12.0))
                        .flex()
                        .rounded(px(3.0))
                        .overflow_hidden()
                        .children(
                            theme
                                .terminal
                                .ansi
                                .iter()
                                .take(8)
                                .map(|color| div().flex_1().h_full().bg(*color)),
                        ),
                ),
        )
        .child(
            div()
                .w(px(84.0))
                .h_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .p(px(8.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .h(px(12.0))
                        .rounded(px(3.0))
                        .bg(theme.diff_add.opacity(0.35)),
                )
                .child(
                    div()
                        .h(px(12.0))
                        .rounded(px(3.0))
                        .bg(theme.diff_del.opacity(0.35)),
                )
                .child(div().h(px(12.0)).rounded(px(3.0)).bg(theme.accent_wash)),
        )
        .into_any_element()
}

pub(super) fn report_panel(theme: &Theme, report: &ImportReport) -> gpui::Stateful<gpui::Div> {
    let summary = format!(
        "{} mapped · {} adjusted · {} inferred/fallback · {} unsupported · {} warnings · {} validation",
        report.mappings.len(),
        report.adjustments.len(),
        report.fallbacks.len(),
        report.dropped.len(),
        report.warnings.len(),
        report.validation.len(),
    );
    div()
        .id(SharedString::from(format!(
            "theme-report-{}",
            report.source_hash
        )))
        .mt(px(8.0))
        .w_full()
        .max_h(px(168.0))
        .overflow_y_scroll()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_raised.opacity(0.35))
        .p(px(10.0))
        .text_size(crate::typography::ui_rems(11.0))
        .line_height(px(16.0))
        .text_color(theme.text_muted)
        .child(div().text_color(theme.text).child(summary))
        .children(report.adjustments.iter().map(|adjustment| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "Adjusted · {} {} → {} · {}",
                adjustment.holt_role, adjustment.original, adjustment.resolved, adjustment.reason
            )))
        }))
        .children(report.fallbacks.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Fallback · {message}")))
        }))
        .children(report.warnings.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Warning · {message}")))
        }))
        .children(report.validation.iter().map(|issue| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "Validation {:?} {:?} · {}",
                issue.category, issue.severity, issue.message
            )))
        }))
        .children(report.dropped.iter().map(|message| {
            div()
                .mt(px(4.0))
                .child(SharedString::from(format!("Unsupported · {message}")))
        }))
        .children(report.mappings.iter().map(|mapping| {
            div().mt(px(4.0)).child(SharedString::from(format!(
                "{} ← {}",
                mapping.holt_role, mapping.vscode_key
            )))
        }))
}
