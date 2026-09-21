//! Provider panel rows: the key input, model list, hidden models,
//! variant switching, and the per-provider/global resets.

use super::*;

/// One hidden-model row as `ListHiddenModels` reports it.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct HiddenModel {
    id: String,
    label: Option<String>,
}

pub(super) fn provider_controls_height(
    models: &Loadable<Vec<Model>>,
    hidden_count: usize,
    hidden_expanded: bool,
    variants: bool,
) -> f32 {
    let list_height = match models {
        Loadable::Idle | Loadable::Loading => 138.0,
        Loadable::Error(_) => 40.0,
        Loadable::Ready(models) if models.is_empty() => 32.0,
        Loadable::Ready(models) => (models.len() as f32 * 32.0).min(192.0),
    };
    // The Hidden block's header always shows; its rows only when expanded.
    let hidden_height = if hidden_count > 0 {
        30.0 + if hidden_expanded {
            (hidden_count as f32 * 32.0).min(96.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    // The key section is label + full-width input + its own Save/Remove
    // row. The danger row is always mounted; the hidden block adds its own
    // height when present. The Add-model action rides the Models header.
    180.0 + list_height + hidden_height + if variants { 34.0 } else { 0.0 } + 44.0
}

pub(super) fn mark_provider_loading(providers: &mut Loadable<Vec<Provider>>) {
    if !matches!(providers, Loadable::Ready(_)) {
        *providers = Loadable::Loading;
    }
}

/// The variant pills at the top of an expanded organization card — the region
/// / edition picker that decides which concrete provider the key and model
/// sections below act on. Absent for single-variant rows.
pub(super) fn variant_selector(
    provider: &Provider,
    active_variant: &str,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> Option<AnyElement> {
    if provider.variants.len() <= 1 {
        return None;
    }
    let org_id = provider.id.to_string();
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(widgets::field_label(theme, "Provider"));
    for variant in &provider.variants {
        let variant_id = variant.id.to_string();
        let selected = variant_id == active_variant;
        let hover_theme = theme.clone();
        let mut pill = div()
            .id(SharedString::from(format!(
                "provider-variant-{org_id}-{variant_id}"
            )))
            .cursor_pointer()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .text_size(crate::typography::ui_rems(11.5))
            .child(SharedString::from(variant.name.clone()));
        pill = if selected {
            pill.border_color(theme.border_strong)
                .bg(crate::theme::ink(0.05))
                .text_color(theme.text)
        } else {
            pill.border_color(theme.border)
                .text_color(theme.text_muted)
                .hover(move |style| {
                    style
                        .bg(crate::theme::ink(0.03))
                        .text_color(hover_theme.text)
                })
        };
        row = row.child(pill.on_click(cx.listener({
            let org_id = org_id.clone();
            let variant_id = variant_id.clone();
            move |page, _, _, cx| page.switch_variant(org_id.clone(), variant_id.clone(), cx)
        })));
    }
    Some(row.into_any_element())
}

pub(super) fn provider_model_list(
    index: usize,
    provider_id: &str,
    models: Loadable<Vec<Model>>,
    theme: &Theme,
    page: gpui::Entity<ProvidersPage>,
    cx: &mut gpui::App,
) -> AnyElement {
    match models {
        Loadable::Idle | Loadable::Loading => {
            popover::skeleton_rows("provider-model-skeleton", theme, 4, page.entity_id(), cx)
        }
        Loadable::Error(error) => widgets::error_strip(theme, error).into_any_element(),
        Loadable::Ready(models) if models.is_empty() => div()
            .py(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child("No models available")
            .into_any_element(),
        Loadable::Ready(models) => {
            let count = models.len();
            let height = (count as f32 * 32.0).min(192.0);
            let provider_prefix = format!("{provider_id}/");
            let provider_for_removal = provider_id.to_string();
            let models = Arc::new(models);
            let row_models = Arc::clone(&models);
            let row_theme = theme.clone();
            gpui::uniform_list(
                ("provider-model-list", index),
                count,
                move |range, _window, cx| {
                    page.update(cx, |_page, cx| {
                        range
                            .filter_map(|row| row_models.get(row))
                            .map(|model| {
                                let raw_id =
                                    model.id.strip_prefix(&provider_prefix).unwrap_or(&model.id);
                                let mut list_row = div()
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .child(
                                        div()
                                            .w(px(200.0))
                                            .flex_none()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(12.0))
                                            .text_color(row_theme.text)
                                            .child(SharedString::from(model.label.clone())),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(row_theme.font_mono.clone())
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(row_theme.text_muted)
                                            .child(SharedString::from(raw_id.to_string())),
                                    );
                                // Only user-added rows are deletable; builtin
                                // catalog rows get the hide action instead —
                                // hiding is reversible, deleting a catalog
                                // entry is not expressible.
                                if model.custom {
                                    let provider = provider_for_removal.clone();
                                    let model_id = model.id.clone();
                                    let idle_icon = row_theme.text_muted;
                                    let hover_icon = row_theme.text;
                                    list_row = list_row.child(
                                        widgets::ghost_action(&row_theme)
                                            .id((
                                                gpui::ElementId::from(("remove-model", index)),
                                                raw_id.to_string(),
                                            ))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.remove_model(
                                                    provider.clone(),
                                                    model_id.clone(),
                                                    cx,
                                                )
                                            }))
                                            // Svg reads only its own text color,
                                            // so the tint and hover live on the
                                            // icon — no background wash.
                                            .child(
                                                crate::icons::icon(crate::icons::CLOSE)
                                                    .size(px(12.0))
                                                    .text_color(idle_icon)
                                                    .hover(move |style| {
                                                        style.text_color(hover_icon)
                                                    }),
                                            ),
                                    );
                                } else {
                                    let provider = provider_for_removal.clone();
                                    let model_id = model.id.clone();
                                    let idle_icon = row_theme.text_muted;
                                    let hover_icon = row_theme.text;
                                    list_row = list_row.child(
                                        widgets::ghost_action(&row_theme)
                                            .id((
                                                gpui::ElementId::from(("hide-model", index)),
                                                raw_id.to_string(),
                                            ))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.hide_model(
                                                    provider.clone(),
                                                    model_id.clone(),
                                                    cx,
                                                )
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::EYE_SLASH)
                                                    .size(px(13.0))
                                                    .text_color(idle_icon)
                                                    .hover(move |style| {
                                                        style.text_color(hover_icon)
                                                    }),
                                            ),
                                    );
                                }
                                list_row.into_any_element()
                            })
                            .collect()
                    })
                },
            )
            .h(px(height))
            .w_full()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .into_any_element()
        }
    }
}

/// The panel's bottom row: reset to the catalog (two-step), and for custom
/// providers the definition removal (two-step; the armed copy names what
/// stays behind — the key and model records survive a definition removal).
pub(super) fn panel_danger_row(
    index: usize,
    provider_id: &str,
    custom: bool,
    armed: bool,
    remove_armed: bool,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let reset_provider = provider_id.to_string();
    let remove_provider = provider_id.to_string();
    let remove_org = provider_id.to_string();
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            widgets::ghost_action(theme)
                .id(("reset-provider", index))
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(
                    cx.listener(move |page, _, _, cx| {
                        page.arm_or_reset(reset_provider.clone(), cx)
                    }),
                )
                .child(if armed {
                    "Confirm reset — drops this provider's user-written entries"
                } else {
                    "Reset to catalog"
                }),
        )
        .children(custom.then(|| {
            widgets::ghost_action(theme)
                .id(("remove-custom-provider", index))
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.arm_or_remove(remove_provider.clone(), remove_org.clone(), cx)
                }))
                .child(if remove_armed {
                    "Confirm remove — the API key and model records stay"
                } else {
                    "Remove provider"
                })
        }))
        .into_any_element()
}

/// The global reset's confirm dialog (the archived page's clear-all
/// pattern): destructive, counted-out, explicit.
pub(super) fn reset_all_dialog(theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    popover::dialog_card(theme)
        .child(popover::dialog_title(theme, "Reset all providers?"))
        .child(div().mt(px(6.0)).child(popover::dialog_body(
            theme,
            "Custom models, model records, hidden lists, and endpoint overrides return to \
                 the compiled catalog for every provider. API keys are kept. This can\u{2019}t \
                 be undone.",
        )))
        .child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_ghost(theme, "Cancel", "reset-all-cancel")
                        .id("reset-all-cancel")
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.confirm_reset_all = false;
                            cx.notify();
                        })),
                )
                .child(
                    popover::btn_danger(theme, "Reset all")
                        .id("reset-all-confirm")
                        .debug_selector(|| "reset-all-confirm".into())
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.confirm_reset_all = false;
                            page.reset_all(cx);
                        })),
                ),
        )
        .into_any_element()
}

/// The hidden block: a collapsible header (chevron + count, collapsed by
/// default) over one greyed row per hidden model with an unhide action.
/// `None` when the provider hides nothing.
pub(super) fn hidden_rows(
    index: usize,
    provider_id: &str,
    hidden: Loadable<Vec<HiddenModel>>,
    expanded: bool,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> Option<AnyElement> {
    let rows = hidden.ready()?;
    if rows.is_empty() {
        return None;
    }
    let count = rows.len();
    let toggle_id = provider_id.to_string();
    let chevron = if expanded {
        crate::icons::ALT_ARROW_DOWN
    } else {
        crate::icons::ALT_ARROW_RIGHT
    };
    let header = div()
        .id(("hidden-header", index))
        .cursor_pointer()
        .w_auto()
        .flex()
        .items_center()
        .gap(px(6.0))
        .on_click(cx.listener(move |page, _, _, cx| page.toggle_hidden(toggle_id.clone(), cx)))
        .child(
            crate::icons::icon(chevron)
                .size(px(11.0))
                .text_color(theme.text_muted),
        )
        .child(widgets::field_label(theme, "Hidden"))
        .child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(format!("{count}"))),
        );
    let block = div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(6.0))
        .child(header);
    if !expanded {
        return Some(block.into_any_element());
    }
    let height = (count as f32 * 32.0).min(96.0);
    let provider_prefix = format!("{provider_id}/");
    let provider = provider_id.to_string();
    let rows = Arc::new(rows.clone());
    let row_theme = theme.clone();
    let page = cx.entity();
    let list = gpui::uniform_list(
        ("provider-hidden-list", index),
        count,
        move |range, _window, cx| {
            page.update(cx, |_page, cx| {
                range
                    .filter_map(|row| rows.get(row))
                    .map(|model| {
                        let raw_id = model.id.strip_prefix(&provider_prefix).unwrap_or(&model.id);
                        let unhide_provider = provider.clone();
                        let unhide_id = model.id.clone();
                        let idle = row_theme.text_muted.opacity(0.55);
                        let hover = row_theme.text_muted;
                        div()
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .opacity(0.55)
                            .child(
                                div()
                                    .w(px(200.0))
                                    .flex_none()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(row_theme.text_muted)
                                    .child(SharedString::from(
                                        model.label.clone().unwrap_or_else(|| raw_id.to_string()),
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(row_theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(row_theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(raw_id.to_string())),
                            )
                            .child(
                                widgets::ghost_action(&row_theme)
                                    .id((
                                        gpui::ElementId::from(("unhide-model", index)),
                                        raw_id.to_string(),
                                    ))
                                    .hover(move |style| style.text_color(hover))
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.unhide_model(
                                            unhide_provider.clone(),
                                            unhide_id.clone(),
                                            cx,
                                        )
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::EYE)
                                            .size(px(13.0))
                                            .text_color(idle),
                                    ),
                            )
                            .into_any_element()
                    })
                    .collect()
            })
        },
    )
    .h(px(height))
    .w_full()
    .occlude()
    .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
    Some(block.child(list).into_any_element())
}

impl ProvidersPage {
    pub(super) fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        // Keep an already-rendered catalog mounted while refreshing it. Save
        // and Remove call this path while a provider panel may be expanded;
        // replacing Ready with Loading would unmount that panel and replay its
        // open animation when the RPC returns.
        mark_provider_loading(&mut self.providers);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_PROVIDERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.providers = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The concrete provider the expanded card acts on: the remembered pick
    /// or the organization's first variant. Every per-provider RPC (save /
    /// reveal / remove key, list models, add model) addresses this id.
    pub(super) fn active_variant_id(&self, org_id: &str) -> Option<String> {
        if let Some(picked) = self.selected_variant.get(org_id) {
            return Some(picked.clone());
        }
        self.providers
            .ready()?
            .iter()
            .find(|row| row.id.as_str() == org_id)?
            .variants
            .first()
            .map(|variant| variant.id.to_string())
    }

    pub(super) fn ensure_variant_inputs(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        self.inputs
            .entry(variant_id.to_string())
            .or_insert_with(|| cx.new(|cx| ComposerInput::new_secret("API key", cx)));
    }

    /// Fetch the stored key and populate the variant's input. A return visit
    /// recreates the page with empty inputs, so without this the saved key is
    /// invisible. Only fills an untouched input — a fetch that lands after the
    /// user started typing (or already holds a draft) must not clobber it.
    pub(super) fn load_key(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let variant = variant_id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REVEAL_PROVIDER_KEY,
                    serde_json::json!({"providerId": variant.clone()}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => {
                        let key = value
                            .get("key")
                            .and_then(serde_json::Value::as_str)
                            .filter(|key| !key.is_empty());
                        if let Some(key) = key
                            && let Some(input) = page.inputs.get(&variant)
                            && input.read(cx).text().is_empty()
                        {
                            input.update(cx, |input, cx| input.set_text(key, cx));
                        }
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        });
        self.key_tasks.insert(variant_id.to_string(), task);
    }

    /// The eye button: flip one variant's input between bullets and plain
    /// text. Purely a projection change — the content (and what Save writes)
    /// is identical either way.
    pub(super) fn toggle_mask(&mut self, variant_id: String, cx: &mut Context<Self>) {
        let unmask = !self.revealed.contains(&variant_id);
        if unmask {
            self.revealed.insert(variant_id.clone());
        } else {
            self.revealed.remove(&variant_id);
        }
        if let Some(input) = self.inputs.get(&variant_id) {
            input.update(cx, |input, cx| input.set_masked(!unmask, cx));
        }
        cx.notify();
    }

    pub(super) fn toggle(&mut self, org_id: &str, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(org_id) {
            self.expanded = None;
            self.conceal_keys(cx);
            self.close_panel_forms();
            self.begin_collapse(org_id.to_string(), cx);
        } else {
            if let Some(previous) = self.expanded.take() {
                self.close_panel_forms();
                self.begin_collapse(previous, cx);
            }
            if self.collapsing.as_deref() == Some(org_id) {
                self.collapsing = None;
            }
            self.expanded = Some(org_id.to_string());
            self.conceal_keys(cx);
            self.close_panel_forms();
            *self.panel_epochs.entry(org_id.to_string()).or_default() += 1;
            if let Some(variant_id) = self.active_variant_id(org_id) {
                self.ensure_variant_inputs(&variant_id, cx);
                self.load_key(&variant_id, cx);
                self.load_models(&variant_id, false, cx);
                self.load_hidden(&variant_id, false, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn switch_variant(
        &mut self,
        org_id: String,
        variant_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_variant.get(&org_id).map(String::as_str) == Some(variant_id.as_str()) {
            return;
        }
        self.selected_variant.insert(org_id, variant_id.clone());
        self.conceal_keys(cx);
        self.close_panel_forms();
        self.ensure_variant_inputs(&variant_id, cx);
        self.load_key(&variant_id, cx);
        self.load_models(&variant_id, false, cx);
        self.load_hidden(&variant_id, false, cx);
        cx.notify();
    }

    /// Retire the per-panel transients: the record form and any armed
    /// reset belong to the variant the panel was showing.
    pub(super) fn close_panel_forms(&mut self) {
        self.record_form = None;
        self.armed_reset = None;
        self.armed_remove = None;
        self.hidden_expanded.clear();
    }

    /// The Hidden block's collapse state flips per variant.
    pub(super) fn toggle_hidden(&mut self, variant_id: String, cx: &mut Context<Self>) {
        if !self.hidden_expanded.insert(variant_id.clone()) {
            self.hidden_expanded.remove(&variant_id);
        }
        cx.notify();
    }

    pub(super) fn begin_collapse(&mut self, provider: String, cx: &mut Context<Self>) {
        self.collapsing = Some(provider.clone());
        *self.panel_epochs.entry(provider.clone()).or_default() += 1;
        let delay = if cx.reduce_motion() {
            std::time::Duration::ZERO
        } else {
            motion::PROVIDER_COLLAPSE
                .total()
                .mul_f32(motion::speed_scale())
        };
        self.collapse_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |page, cx| {
                if page.collapsing.as_deref() == Some(provider.as_str()) {
                    page.collapsing = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub(super) fn load_models(&mut self, provider: &str, force: bool, cx: &mut Context<Self>) {
        if !force && matches!(self.models.get(provider), Some(Loadable::Ready(_))) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let provider = provider.to_string();
        if !matches!(self.models.get(&provider), Some(Loadable::Ready(_))) {
            self.models.insert(provider.clone(), Loadable::Loading);
        }
        let request_provider = provider.clone();
        let slot_provider = provider.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_MODELS,
                    serde_json::json!({"providerId": request_provider}),
                )
                .await;
            this.update(cx, |page, cx| {
                let models = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.models.insert(slot_provider, models);
                cx.notify();
            })
            .ok();
        });
        self.model_tasks.insert(provider, task);
    }

    pub(super) fn remove_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_MODEL_RECORD,
                    serde_json::json!({"providerId": provider, "modelId": model}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn load_hidden(&mut self, provider: &str, force: bool, cx: &mut Context<Self>) {
        if !force && matches!(self.hidden.get(provider), Some(Loadable::Ready(_))) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let provider = provider.to_string();
        self.hidden.insert(provider.clone(), Loadable::Loading);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_HIDDEN_MODELS,
                    serde_json::json!({ "providerId": provider.clone() }),
                )
                .await;
            this.update(cx, |page, cx| {
                let rows = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.hidden.insert(provider, rows);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Hides one catalog row: the engine takes the provider's whole hidden
    /// set, so the new set is the loaded one plus the id.
    pub(super) fn hide_model(
        &mut self,
        provider: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let mut ids: Vec<String> = self
            .hidden
            .get(&provider)
            .and_then(Loadable::ready)
            .map(|rows| rows.iter().map(|row| row.id.clone()).collect())
            .unwrap_or_default();
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
        self.set_hidden(provider, ids, cx);
    }

    pub(super) fn unhide_model(
        &mut self,
        provider: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<String> = self
            .hidden
            .get(&provider)
            .and_then(Loadable::ready)
            .map(|rows| {
                rows.iter()
                    .map(|row| row.id.clone())
                    .filter(|id| id != &model_id)
                    .collect()
            })
            .unwrap_or_default();
        self.set_hidden(provider, ids, cx);
    }

    pub(super) fn set_hidden(
        &mut self,
        provider: String,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_HIDDEN_MODELS,
                    serde_json::json!({
                        "providerId": provider.clone(),
                        "modelIds": model_ids,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Two-step per-provider reset: first click arms, second executes.
    pub(super) fn arm_or_reset(&mut self, provider: String, cx: &mut Context<Self>) {
        if self.armed_reset.as_deref() == Some(provider.as_str()) {
            self.armed_reset = None;
            self.reset_provider(provider, cx);
        } else {
            self.armed_reset = Some(provider);
            cx.notify();
        }
    }

    pub(super) fn arm_or_remove(
        &mut self,
        provider: String,
        org_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.armed_remove.as_deref() == Some(provider.as_str()) {
            self.armed_remove = None;
            self.remove_custom_provider(provider, org_id, cx);
        } else {
            self.armed_remove = Some(provider);
            cx.notify();
        }
    }

    pub(super) fn reset_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RESET_PROVIDER_CATALOG,
                    serde_json::json!({ "providerId": provider }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn reset_all(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::RESET_PROVIDER_CATALOG, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        let refresh: Vec<String> = page.models.keys().cloned().collect();
                        for provider in refresh {
                            page.load_models(&provider, true, cx);
                            page.load_hidden(&provider, true, cx);
                        }
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn save(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self
            .inputs
            .get(&provider)
            .map(|input| input.read(cx).text().to_string())
            .unwrap_or_default();
        if key.trim().is_empty() {
            self.fail("API key is required", cx);
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_PROVIDER_KEY,
                    serde_json::json!({"providerId": provider, "key": key}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn remove(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_PROVIDER_KEY,
                    serde_json::json!({"providerId": provider}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.conceal_keys(cx);
                        if let Some(input) = page.inputs.get(&provider) {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.fail(error.to_string(), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_keeps_ready_provider_rows_mounted() {
        let mut providers = Loadable::Ready(Vec::<Provider>::new());

        mark_provider_loading(&mut providers);

        assert!(matches!(providers, Loadable::Ready(_)));
    }

    #[test]
    fn panel_heights_account_for_the_new_sections() {
        let models = Loadable::Ready(Vec::<Model>::new());
        let bare = provider_controls_height(&models, 0, false, false);
        // Hidden rows and their absence move the panel's animated height;
        // the collapsed Hidden block adds only its header.
        let collapsed = provider_controls_height(&models, 2, false, false);
        let expanded = provider_controls_height(&models, 2, true, false);
        assert!(collapsed > bare);
        assert!(expanded > collapsed);
    }
}
