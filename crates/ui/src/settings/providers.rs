//! Settings → Providers: the provider list and the Add Provider dialog.
//!
//! `panels` — provider rows: keys, model list, hidden models, resets
//! (ADR-0028's catalog layers).
//! `record_form` — the manual model-record dialog (ADR-0029).
//! `add_dialog` — the Add Provider dialog and the manual definition
//! form; "Add with AI" hands off to a Provider Mode chat (ADR-0037).

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
use serde::Deserialize;

use crate::{
    composer::ComposerInput,
    motion::{self, AnimationExt as _},
    popover::{self, Loadable, Popup},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

mod add_dialog;
mod panels;
mod record_form;

use add_dialog::*;
use panels::*;
use record_form::*;

#[cfg(test)]
mod test_support;

/// Surfaced to the shell, which renders it as a window-top modal — action
/// failures (save/remove key, RPC errors) never paint inside the page.
#[derive(Debug, Clone)]
pub enum ProvidersPageEvent {
    Error(SharedString),
    /// "Add with AI": open a new chat in Provider Mode (ADR-0037).
    StartProviderChat,
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
    model_tasks: HashMap<String, Task<()>>,
    /// Per-variant reveal tasks: the panel expansion fires the reveal and
    /// the hidden-models fetch together, and both used to share the page
    /// task slot — the second spawn cancelled the first, so a stored key
    /// never filled its input on a first expansion.
    key_tasks: HashMap<String, Task<()>>,
    /// Variants whose API-key input is currently unmasked; everything starts
    /// masked on every expansion and re-masks when the panel collapses or the
    /// page is left.
    revealed: HashSet<String>,
    /// The Add Provider dialog (the manual form) is open.
    add_dialog: bool,
    new_provider_inputs: HashMap<&'static str, Entity<ComposerInput>>,
    new_provider_error: Option<String>,
    /// The mounted model-record form, targeting the expanded panel's active
    /// variant (ADR-0029's manual half of the write path).
    record_form: Option<RecordForm>,
    /// The record form's dialect dropdown; page-level because the form is
    /// an Option the popup accessor can't borrow through.
    record_api_menu: Popup<()>,
    /// The engine's registered API dialects (the dropdown's options),
    /// loaded once per page.
    api_dialects: Loadable<Vec<String>>,
    /// Hidden rows per variant, loaded beside the model list.
    hidden: HashMap<String, Loadable<Vec<HiddenModel>>>,
    /// Variants whose Hidden block is expanded; it starts collapsed on
    /// every panel expansion (like the masked key).
    hidden_expanded: HashSet<String>,
    /// Two-step per-provider reset confirmation: the first click arms, the
    /// second executes. The GLOBAL reset rides a confirm dialog instead
    /// (`confirm_reset_all`) — the armed-button copy never fit the row.
    armed_reset: Option<String>,
    /// The same two-step for removing a custom provider's definition —
    /// the armed copy states what stays behind (the key, the records).
    armed_remove: Option<String>,
    /// The global reset's confirm dialog is open.
    confirm_reset_all: bool,
    task: Option<Task<()>>,
    collapse_task: Option<Task<()>>,
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    let models = self
                        .models
                        .get(&variant_id)
                        .cloned()
                        .unwrap_or(Loadable::Idle);
                    let hidden = self
                        .hidden
                        .get(&variant_id)
                        .cloned()
                        .unwrap_or(Loadable::Idle);
                    let hidden_count = hidden.ready().map(|rows| rows.len()).unwrap_or(0);
                    let hidden_expanded = self.hidden_expanded.contains(&variant_id);
                    let revealed = self.revealed.contains(&variant_id);
                    let panel_height = provider_controls_height(
                        &models,
                        hidden_count,
                        hidden_expanded,
                        provider.variants.len() > 1,
                    );
                    let panel_epoch = self.panel_epochs.get(&id).copied().unwrap_or_default();
                    let save_id = variant_id.clone();
                    let remove_id = variant_id.clone();
                    let model_count = models.ready().map(|models| models.len());
                    let danger = theme.danger;
                    let danger_muted = theme.danger_muted;
                    let model_list =
                        provider_model_list(index, &variant_id, models, &theme, cx.entity(), cx);
                    let hidden_list =
                        hidden_rows(index, &variant_id, hidden, hidden_expanded, &theme, cx);
                    let add_record_id = variant_id.clone();
                    let danger_row = panel_danger_row(
                        index,
                        &variant_id,
                        provider.custom,
                        self.armed_reset.as_deref() == Some(variant_id.as_str()),
                        self.armed_remove.as_deref() == Some(variant_id.as_str()),
                        &theme,
                        cx,
                    );
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
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(widgets::field_label(&theme, "Models"))
                                        .children(model_count.map(|count| {
                                            div()
                                                .text_size(crate::typography::ui_rems(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(format!("{count}")))
                                                .into_any_element()
                                        }))
                                        .child(div().flex_1())
                                        .child(
                                            widgets::ghost_action(&theme)
                                                .id(("toggle-record-form", index))
                                                .debug_selector(|| "toggle-record-form".into())
                                                .hover(|style| style.bg(crate::theme::ink(0.04)))
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.open_record_form(
                                                        add_record_id.clone(),
                                                        cx,
                                                    );
                                                }))
                                                .child(
                                                    crate::icons::icon(crate::icons::PLUS)
                                                        .size(px(12.0))
                                                        .text_color(theme.text_muted),
                                                )
                                                .child("Add model"),
                                        ),
                                )
                                .child(model_list),
                        )
                        .children(hidden_list)
                        .child(danger_row);
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
                    // The row's affordance: the whole row toggles, so the
                    // chevron carries the expanded/collapsing state.
                    crate::icons::icon(if panel_mounted {
                        crate::icons::ALT_ARROW_DOWN
                    } else {
                        crate::icons::ALT_ARROW_RIGHT
                    })
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted)
                    .into_any_element(),
                ];
                div()
                    .flex()
                    .flex_col()
                    .child(
                        widgets::flat_row()
                            .id(("provider-row", index))
                            .debug_selector(move || format!("provider-row-{index}"))
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
            Loadable::Ready(_) => {
                // Materialized first: the row builder borrows `cx` lazily,
                // and the action row needs it mutably next.
                let rows: Vec<AnyElement> = rows.collect();
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(top_action_row(&theme, cx))
                    .children(rows)
                    .into_any_element()
            }
        };
        let page = div()
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
            );
        // The page hosts its own deferred-layer modals (the appearance
        // library's review dialog does the same).
        if self.confirm_reset_all {
            let card = reset_all_dialog(&theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "reset-all-providers-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        if self.add_dialog {
            let card = add_provider_dialog(self, &theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "add-provider-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        if self.record_form.is_some() {
            let card = record_form_dialog(self, &theme, cx);
            return div()
                .child(page)
                .child(popover::modal(
                    "record-form-dialog",
                    window.viewport_size(),
                    card,
                ))
                .into_any_element();
        }
        page.into_any_element()
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

impl Drop for ProvidersPage {
    fn drop(&mut self) {
        self.revealed.clear();
    }
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
            model_tasks: HashMap::new(),
            key_tasks: HashMap::new(),
            revealed: HashSet::new(),
            add_dialog: false,
            new_provider_inputs: HashMap::new(),
            new_provider_error: None,
            record_form: None,
            record_api_menu: Popup::default(),
            api_dialects: Loadable::Idle,
            hidden: HashMap::new(),
            hidden_expanded: HashSet::new(),
            armed_reset: None,
            armed_remove: None,
            confirm_reset_all: false,
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
}
