use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Task, Window,
    div, prelude::*, px,
};
use holt_proto::{Model, Provider};
use holt_rpc::methods;

use crate::{
    composer::ComposerInput,
    motion::{self, AnimationExt as _},
    popover::{self, Loadable},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

/// Surfaced to the shell, which renders it as a window-top modal — action
/// failures (save/remove key, RPC errors) never paint inside the page.
#[derive(Debug, Clone)]
pub enum ProvidersPageEvent {
    Error(SharedString),
}

impl EventEmitter<ProvidersPageEvent> for ProvidersPage {}

pub struct ProvidersPage {
    state: Entity<AppState>,
    providers: Loadable<Vec<Provider>>,
    expanded: Option<String>,
    collapsing: Option<String>,
    panel_epochs: HashMap<String, u64>,
    /// Per-organization selected variant (concrete provider id); falls back
    /// to the first variant when unset.
    selected_variant: HashMap<String, String>,
    inputs: HashMap<String, Entity<ComposerInput>>,
    models: HashMap<String, Loadable<Vec<Model>>>,
    model_inputs: HashMap<String, Entity<ComposerInput>>,
    /// Per-variant hint for a rejected "Add model" attempt (duplicate ID,
    /// engine rejection) — small inline text, not the page error strip.
    model_errors: HashMap<String, String>,
    model_tasks: HashMap<String, Task<()>>,
    /// Variants whose API-key input is currently unmasked; everything starts
    /// masked on every expansion and re-masks when the panel collapses or the
    /// page is left.
    revealed: HashSet<String>,
    task: Option<Task<()>>,
    collapse_task: Option<Task<()>>,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            providers: Loadable::Idle,
            expanded: None,
            collapsing: None,
            panel_epochs: HashMap::new(),
            selected_variant: HashMap::new(),
            inputs: HashMap::new(),
            models: HashMap::new(),
            model_inputs: HashMap::new(),
            model_errors: HashMap::new(),
            model_tasks: HashMap::new(),
            revealed: HashSet::new(),
            task: None,
            collapse_task: None,
        };
        page.load(cx);
        page
    }

    /// Re-mask every key input and forget which were unmasked — called when
    /// the panel collapses, the variant switches, or the page is left.
    fn conceal_keys(&mut self, cx: &mut Context<Self>) {
        self.revealed.clear();
        for input in self.inputs.values() {
            input.update(cx, |input, cx| input.set_masked(true, cx));
        }
    }

    /// The shell's leave-the-page hook: the page entity outlives the visit.
    pub fn clear_revealed(&mut self, cx: &mut Context<Self>) {
        self.conceal_keys(cx);
        cx.notify();
    }

    /// Surface a failure to the shell's window-top error modal.
    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        cx.emit(ProvidersPageEvent::Error(message.into()));
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
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
    fn active_variant_id(&self, org_id: &str) -> Option<String> {
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

    fn ensure_variant_inputs(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        self.inputs
            .entry(variant_id.to_string())
            .or_insert_with(|| cx.new(|cx| ComposerInput::new_secret("API key", cx)));
        self.model_inputs
            .entry(variant_id.to_string())
            .or_insert_with(|| cx.new(|cx| ComposerInput::new("Model ID", cx)));
    }

    /// Fetch the stored key and populate the variant's input. A return visit
    /// recreates the page with empty inputs, so without this the saved key is
    /// invisible. Only fills an untouched input — a fetch that lands after the
    /// user started typing (or already holds a draft) must not clobber it.
    fn load_key(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let variant = variant_id.to_string();
        self.task = Some(cx.spawn(async move |this, cx| {
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
        }));
    }

    /// The eye button: flip one variant's input between bullets and plain
    /// text. Purely a projection change — the content (and what Save writes)
    /// is identical either way.
    fn toggle_mask(&mut self, variant_id: String, cx: &mut Context<Self>) {
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

    fn toggle(&mut self, org_id: &str, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(org_id) {
            self.expanded = None;
            self.conceal_keys(cx);
            self.begin_collapse(org_id.to_string(), cx);
        } else {
            if let Some(previous) = self.expanded.take() {
                self.begin_collapse(previous, cx);
            }
            if self.collapsing.as_deref() == Some(org_id) {
                self.collapsing = None;
            }
            self.expanded = Some(org_id.to_string());
            self.conceal_keys(cx);
            *self.panel_epochs.entry(org_id.to_string()).or_default() += 1;
            if let Some(variant_id) = self.active_variant_id(org_id) {
                self.ensure_variant_inputs(&variant_id, cx);
                self.load_key(&variant_id, cx);
                self.load_models(&variant_id, false, cx);
            }
        }
        cx.notify();
    }

    fn switch_variant(&mut self, org_id: String, variant_id: String, cx: &mut Context<Self>) {
        if self.selected_variant.get(&org_id).map(String::as_str) == Some(variant_id.as_str()) {
            return;
        }
        self.selected_variant.insert(org_id, variant_id.clone());
        self.conceal_keys(cx);
        self.ensure_variant_inputs(&variant_id, cx);
        self.load_key(&variant_id, cx);
        self.load_models(&variant_id, false, cx);
        cx.notify();
    }

    fn begin_collapse(&mut self, provider: String, cx: &mut Context<Self>) {
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

    fn load_models(&mut self, provider: &str, force: bool, cx: &mut Context<Self>) {
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

    fn add_model(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let model = self
            .model_inputs
            .get(&provider)
            .map(|input| input.read(cx).text().trim().to_string())
            .unwrap_or_default();
        if model.is_empty() {
            self.model_errors
                .insert(provider, "Model ID is required".to_string());
            cx.notify();
            return;
        }
        self.model_errors.remove(&provider);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::ADD_PROVIDER_MODEL,
                    serde_json::json!({"providerId": provider, "modelId": model}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.model_errors.remove(&provider);
                        if let Some(input) = page.model_inputs.get(&provider) {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                    }
                    Err(error) => {
                        page.model_errors
                            .insert(provider.clone(), error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove_model(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_PROVIDER_MODEL,
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

    fn save(&mut self, provider: String, cx: &mut Context<Self>) {
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

    fn remove(&mut self, provider: String, cx: &mut Context<Self>) {
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

impl Render for ProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let rows = self
            .providers
            .ready()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, provider)| {
                let id = provider.id.to_string();
                let brand_mark = match crate::pickers::provider_brand_icon(&provider.id) {
                    Some((path, tint)) => crate::icons::icon(path)
                        .size(px(22.))
                        .text_color(tint.unwrap_or(theme.text))
                        .into_any_element(),
                    None => div()
                        .text_center()
                        .child(SharedString::from(provider.abbreviation.clone()))
                        .into_any_element(),
                };
                let expanded = self.expanded.as_deref() == Some(id.as_str());
                let collapsing = self.collapsing.as_deref() == Some(id.as_str());
                let panel_mounted = expanded || collapsing;
                let status = if provider.configured {
                    "Configured"
                } else {
                    "Not configured"
                };
                let controls: Option<AnyElement> = panel_mounted.then(|| {
                    let variant_id = self.active_variant_id(&id).unwrap_or_else(|| id.clone());
                    let input = self.inputs.get(&variant_id).cloned();
                    let model_input = self.model_inputs.get(&variant_id).cloned();
                    let model_error = self.model_errors.get(&variant_id).cloned();
                    let models = self
                        .models
                        .get(&variant_id)
                        .cloned()
                        .unwrap_or(Loadable::Idle);
                    let revealed = self.revealed.contains(&variant_id);
                    let panel_height = provider_controls_height(
                        &models,
                        provider.variants.len() > 1,
                        model_error.is_some(),
                    );
                    let panel_epoch = self.panel_epochs.get(&id).copied().unwrap_or_default();
                    let save_id = variant_id.clone();
                    let remove_id = variant_id.clone();
                    let add_model_id = variant_id.clone();
                    let model_count = models.ready().map(|models| models.len());
                    let danger = theme.danger;
                    let danger_muted = theme.danger_muted;
                    let model_list =
                        provider_model_list(index, &variant_id, models, &theme, cx.entity(), cx);
                    let variant_selector = variant_selector(&provider, &variant_id, &theme, cx);
                    let content = div()
                        .pl(px(56.0))
                        .pr(px(8.0))
                        .pt(px(8.0))
                        .pb(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(16.0))
                        .children(variant_selector)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(widgets::field_label(&theme, "API key"))
                                .children(input.map(|input| {
                                    secret_field(&theme, input, revealed, index, &variant_id, cx)
                                        .w_full()
                                        .into_any_element()
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            action_button(&theme)
                                                .id(("save-provider", index))
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.save(save_id.clone(), cx)
                                                }))
                                                .child("Save"),
                                        )
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id(("remove-provider", index))
                                                .hover(move |style| {
                                                    style
                                                        .bg(danger.opacity(0.10))
                                                        .text_color(danger_muted)
                                                })
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.remove(remove_id.clone(), cx)
                                                }))
                                                .child("Remove"),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_baseline()
                                        .gap(px(8.0))
                                        .child(widgets::field_label(&theme, "Models"))
                                        .children(model_count.map(|count| {
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(format!("{count}")))
                                                .into_any_element()
                                        })),
                                )
                                .child(model_list)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .children(model_input.map(|input| {
                                            bordered_input(&theme, input)
                                                .flex_1()
                                                .min_w_0()
                                                .into_any_element()
                                        }))
                                        .child(
                                            action_button(&theme)
                                                .id(("add-provider-model", index))
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.add_model(add_model_id.clone(), cx)
                                                }))
                                                .child("Add model"),
                                        ),
                                )
                                .children(model_error.map(|message| {
                                    div()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.danger_muted.opacity(0.9))
                                        .child(SharedString::from(message))
                                })),
                        );
                    let panel = div().w_full().overflow_hidden().child(content);
                    if collapsing {
                        panel
                            .with_animation(
                                ("provider-panel-close", panel_epoch),
                                motion::PROVIDER_COLLAPSE.animation(),
                                move |panel, progress| {
                                    panel
                                        .h(px(motion::lerp(panel_height, 0.0, progress)))
                                        .opacity(1.0 - progress)
                                        .relative()
                                        .top(px(-2.0 * progress))
                                },
                            )
                            .into_any_element()
                    } else {
                        panel
                            .with_animation(
                                ("provider-panel-open", panel_epoch),
                                motion::PROVIDER_EXPAND.animation(),
                                move |panel, progress| {
                                    panel
                                        .h(px(motion::lerp(0.0, panel_height, progress)))
                                        .opacity(progress)
                                        .relative()
                                        .top(px(-4.0 * (1.0 - progress)))
                                },
                            )
                            .into_any_element()
                    }
                });
                let expand_id = id.clone();
                let header_children: Vec<AnyElement> = vec![
                    div()
                        .w(px(36.))
                        .flex()
                        .justify_center()
                        .child(brand_mark)
                        .into_any_element(),
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(widgets::row_title(&theme, provider.name))
                        .child(widgets::row_description(&theme, status))
                        .into_any_element(),
                ];
                div()
                    .flex()
                    .flex_col()
                    .child(
                        widgets::flat_row()
                            .id(("provider-row", index))
                            .cursor_pointer()
                            .px(px(8.0))
                            .rounded(px(8.0))
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .gap(px(12.))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.toggle(&expand_id, cx)),
                            )
                            .children(header_children),
                    )
                    .children(controls)
                    .into_any_element()
            });
        let body = match &self.providers {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("providers-skeleton", &theme, 6, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            Loadable::Ready(_) => div()
                .mt(px(24.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element(),
        };
        div()
            .id("providers-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Providers", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Configure API keys for the providers Holt can use. Organizations with \
                         several endpoints are configured per endpoint.",
                    ))
                    .child(body),
            )
    }
}

/// The API-key field: the bordered input carrying the stored key, masked to
/// bullets, with the in-field eye toggle that flips the projection.
fn secret_field(
    theme: &Theme,
    input: Entity<ComposerInput>,
    revealed: bool,
    index: usize,
    variant_id: &str,
    cx: &mut Context<ProvidersPage>,
) -> gpui::Div {
    let hover_theme = theme.clone();
    let toggle_id = variant_id.to_string();
    div()
        .h(px(36.0))
        .pl(px(12.0))
        .pr(px(6.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        // The input's root is `w_full`: without a shrinkable track it claims
        // the whole content box and pushes the flex-none eye past the border.
        .child(div().flex_1().min_w_0().child(input))
        .child(
            widgets::ghost_action(theme)
                .flex_none()
                .id(("toggle-key-mask", index))
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(
                    cx.listener(move |page, _, _, cx| page.toggle_mask(toggle_id.clone(), cx)),
                )
                .child(
                    crate::icons::icon(if revealed {
                        crate::icons::EYE_SLASH
                    } else {
                        crate::icons::EYE
                    })
                    .size(px(15.0))
                    .text_color(theme.text_muted),
                ),
        )
}

fn bordered_input(theme: &Theme, input: Entity<ComposerInput>) -> gpui::Div {
    div()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        .child(input)
}

/// A bordered button matching [`bordered_input`]'s 36px height, so the two sit
/// flush on the same row. Caller adds id + hover + click.
fn action_button(theme: &Theme) -> gpui::Div {
    div()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .cursor_pointer()
}

fn provider_controls_height(models: &Loadable<Vec<Model>>, variants: bool, hint: bool) -> f32 {
    let list_height = match models {
        Loadable::Idle | Loadable::Loading => 138.0,
        Loadable::Error(_) => 40.0,
        Loadable::Ready(models) if models.is_empty() => 32.0,
        Loadable::Ready(models) => (models.len() as f32 * 32.0).min(192.0),
    };
    // The hint adds one 11px text line plus the section's 8px flex gap; the
    // key section is label + full-width input + its own Save/Remove row.
    224.0 + list_height + if variants { 34.0 } else { 0.0 } + if hint { 24.0 } else { 0.0 }
}

fn mark_provider_loading(providers: &mut Loadable<Vec<Provider>>) {
    if !matches!(providers, Loadable::Ready(_)) {
        *providers = Loadable::Loading;
    }
}

/// The variant pills at the top of an expanded organization card — the region
/// / edition picker that decides which concrete provider the key and model
/// sections below act on. Absent for single-variant rows.
fn variant_selector(
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

fn provider_model_list(
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
                                // catalog rows render without the close action.
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

impl Drop for ProvidersPage {
    fn drop(&mut self) {
        self.revealed.clear();
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
}
