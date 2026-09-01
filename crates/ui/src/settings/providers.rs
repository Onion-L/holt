use std::collections::HashMap;

use gpui::{Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*, px};
use holt_proto::Provider;
use holt_rpc::methods;

use crate::{
    composer::ComposerInput,
    popover::{self, Loadable},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

pub struct ProvidersPage {
    state: Entity<AppState>,
    providers: Loadable<Vec<Provider>>,
    expanded: Option<String>,
    inputs: HashMap<String, Entity<ComposerInput>>,
    revealed: HashMap<String, String>,
    error: Option<String>,
    task: Option<Task<()>>,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            providers: Loadable::Idle,
            expanded: None,
            inputs: HashMap::new(),
            revealed: HashMap::new(),
            error: None,
            task: None,
        };
        page.load(cx);
        page
    }

    pub fn clear_revealed(&mut self, cx: &mut Context<Self>) {
        self.revealed.clear();
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.providers = Loadable::Loading;
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

    fn toggle(&mut self, provider: &str, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(provider) {
            self.expanded = None;
            self.revealed.clear();
        } else {
            self.expanded = Some(provider.to_string());
            self.revealed.clear();
            self.inputs
                .entry(provider.to_string())
                .or_insert_with(|| cx.new(|cx| ComposerInput::new("API key", cx)));
        }
        cx.notify();
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
        self.error = None;
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
                    Err(error) => page.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn reveal(&mut self, provider: String, cx: &mut Context<Self>) {
        if self.revealed.remove(&provider).is_some() {
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REVEAL_PROVIDER_KEY,
                    serde_json::json!({"providerId": provider}),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => {
                        if let Some(key) = value.get("key").and_then(|value| value.as_str()) {
                            page.revealed.insert(provider, key.to_string());
                        }
                    }
                    Err(error) => page.error = Some(error.to_string()),
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
                        page.revealed.clear();
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                    }
                    Err(error) => page.error = Some(error.to_string()),
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
                let expanded = self.expanded.as_deref() == Some(id.as_str());
                let status = if provider.configured {
                    "Configured"
                } else {
                    "Not configured"
                };
                let controls = expanded.then(|| {
                    let input = self.inputs.get(&id).cloned();
                    let revealed = self.revealed.get(&id).cloned();
                    let save_id = id.clone();
                    let reveal_id = id.clone();
                    let remove_id = id.clone();
                    div()
                        .pl(px(52.))
                        .pb(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .children(input.map(|input| input.into_any_element()))
                        .children(revealed.map(|key| {
                            div()
                                .font_family("monospace")
                                .child(SharedString::from(key))
                                .into_any_element()
                        }))
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(
                                    widgets::ghost_action(&theme)
                                        .id(("save-provider", index))
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.save(save_id.clone(), cx)
                                        }))
                                        .child("Save"),
                                )
                                .child(
                                    widgets::ghost_action(&theme)
                                        .id(("reveal-provider", index))
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.reveal(reveal_id.clone(), cx)
                                        }))
                                        .child(if self.revealed.contains_key(&id) {
                                            "Hide"
                                        } else {
                                            "Reveal"
                                        }),
                                )
                                .child(
                                    widgets::ghost_action(&theme)
                                        .id(("remove-provider", index))
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.remove(remove_id.clone(), cx)
                                        }))
                                        .child("Remove"),
                                ),
                        )
                });
                let toggle_id = id.clone();
                div()
                    .flex()
                    .flex_col()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id(("provider-row", index))
                            .cursor_pointer()
                            .p(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.toggle(&toggle_id, cx)),
                            )
                            .child(
                                div()
                                    .w(px(36.))
                                    .text_center()
                                    .child(SharedString::from(provider.abbreviation)),
                            )
                            .child(div().flex_1().child(SharedString::from(provider.name)))
                            .child(div().text_color(theme.text_muted).child(status)),
                    )
                    .children(controls)
            });
        let body = match &self.providers {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("providers-skeleton", &theme, 6, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            Loadable::Ready(_) => widgets::section_card(&theme)
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
                        "Configure one API key for each model provider.",
                    ))
                    .children(
                        self.error
                            .clone()
                            .map(|error| widgets::error_strip(&theme, error)),
                    )
                    .child(body),
            )
    }
}

impl Drop for ProvidersPage {
    fn drop(&mut self) {
        self.revealed.clear();
    }
}
