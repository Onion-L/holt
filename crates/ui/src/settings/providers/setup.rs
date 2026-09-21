//! The AI setup tab (V2c): the session setup chat, its transcript view,
//! the assistant model picker, the proposal review panel, and the Key
//! request card (ADR-0031).

use super::*;

/// The AI tab (V2c): the setup chat's mini transcript, the model picker,
/// and the review panel. Deliberately spare — text rows, tool chips, and
/// proposal cards are the whole surface.
pub(super) fn ai_tab(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let input = page.setup_input.clone();
    let models_ready = matches!(page.setup_models, Loadable::Ready(_));
    let view = page.setup_transcript_view.clone();
    let doc_empty = page
        .setup_chat
        .as_deref()
        .map(|id| page.state.read(cx).sub_transcript(id).is_empty())
        .unwrap_or(true);
    let mut column = div().flex().flex_col().gap(px(10.0)).flex_1().min_h_0();
    if let Loadable::Error(reason) = page.setup_models.clone() {
        // The AI tab needs a working model of its own — a fresh install has
        // none. Guide the way out instead of dead-ending on an error strip:
        // the key lives on this page's provider panels, the manual form is
        // one tab away.
        return column
            .child(widgets::error_strip(theme, reason))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .pt(px(4.0))
                    .max_w(px(420.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(
                        "Enter an API key for any provider in the list behind this \
                         dialog first, or add the provider by hand.",
                    )
                    .child(
                        action_button(theme)
                            .id("setup-bootstrap-manual")
                            .debug_selector(|| "setup-bootstrap-manual".into())
                            .w(px(180.0))
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.add_dialog = Some(AddProviderTab::Manual);
                                cx.notify();
                            }))
                            .child("Use the Manual tab"),
                    ),
            )
            .into_any_element();
    }
    if models_ready || page.setup_chat.is_some() {
        // The chat surface fills the dialog like the new-chat canvas: the
        // transcript grows, the composer card stays pinned at the bottom.
        let surface: AnyElement = if let Some(view) = view.filter(|_| !doc_empty) {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(view)
                .into_any_element()
        } else {
            div()
                .id("setup-empty-state")
                .debug_selector(|| "setup-empty-state".into())
                .flex_1()
                .min_h(px(200.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_align(gpui::TextAlign::Center)
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(
                            "Tell the assistant which provider or model to set up — it \
                             researches the official docs, compares the local catalog, and \
                             prepares a proposal you can review here.",
                        ),
                )
                .into_any_element()
        };
        column = column.child(surface);
        // The review panel pops in when a proposal lands — the whole point
        // of the run — and stays out of the way otherwise.
        if !page.setup_proposals.is_empty() || !page.setup_applied.is_empty() {
            column = column.child(setup_review_panel(page, theme, cx));
        }
        // A turn the engine could not admit never writes a doc entry —
        // without this strip the send reads as dead silence (issue 03).
        if let Some(reason) = page
            .setup_queue
            .as_ref()
            .and_then(setup_queue_error)
            .map(|reason| format!("{reason} Send again to retry."))
        {
            column = column.child(
                widgets::error_strip(theme, reason)
                    .id("setup-queue-error")
                    .debug_selector(|| "setup-queue-error".into()),
            );
        }
        if page.setup_key_request.is_some() {
            column = column.child(setup_key_request_card(page, theme, cx));
        }
        if let Some(input) = input {
            column = column.child(setup_composer(page, theme, input, cx));
        }
    }
    column.into_any_element()
}

/// The Key request card (ADR-0031): the assistant asked for a provider
/// key it cannot see. The destination is the point of the card — saving
/// approves exactly that URL — and the input is masked like every other
/// key field; the value rides the settle RPC and nothing else.
pub(super) fn setup_key_request_card(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let Some(request) = page.setup_key_request.clone() else {
        return div().into_any_element();
    };
    let provider = request["providerName"]
        .as_str()
        .or_else(|| request["providerId"].as_str())
        .unwrap_or("the provider");
    let destination = request["destination"].as_str().unwrap_or_default();
    let has_key = request["hasKey"].as_bool().unwrap_or(false);
    let input = page.setup_key_input.clone();
    let typed = input
        .as_ref()
        .map(|input| !input.read(cx).text().trim().is_empty())
        .unwrap_or(false);
    let settling = page.setup_key_settling;
    let error = page.setup_key_error.clone();
    let mut card = div()
        .id("setup-key-request-card")
        .debug_selector(|| "setup-key-request-card".into())
        .rounded(px(12.0))
        .bg(theme.input_glass_bg())
        .border_1()
        .border_color(theme.border)
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text)
                        .child(SharedString::from(format!(
                            "API key requested for {provider}"
                        ))),
                )
                .child(
                    div()
                        .id("setup-key-request-destination")
                        .debug_selector(|| "setup-key-request-destination".into())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(SharedString::from(destination.to_string())),
                ),
        )
        .child(
            div()
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text_muted.opacity(0.8))
                .child(
                    "Saved locally — sent only to this destination when listing models, never to the chat.",
                ),
        )
        .children(has_key.then(|| {
            div()
                .id("setup-key-request-existing")
                .debug_selector(|| "setup-key-request-existing".into())
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text_muted.opacity(0.8))
                .child("A key is already stored — saving replaces it.")
        }))
        .children(input)
        .children(error.map(|message| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.danger_muted.opacity(0.9))
                .child(SharedString::from(message))
        }));
    if !settling {
        card =
            card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        action_button(theme)
                            .id("setup-key-request-save")
                            .debug_selector(|| "setup-key-request-save".into())
                            .hover(|style| style.bg(crate::theme::ink(0.04)))
                            .when(!typed, |el| el.opacity(0.35))
                            .when(typed, |el| {
                                el.cursor_pointer().on_click(cx.listener(|page, _, _, cx| {
                                    page.settle_setup_key_request(true, cx)
                                }))
                            })
                            .child("Save key"),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("setup-key-request-dismiss")
                            .debug_selector(|| "setup-key-request-dismiss".into())
                            .hover(move |style| widgets::ghost_hover(theme, style))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.settle_setup_key_request(false, cx)
                            }))
                            .child("Dismiss"),
                    ),
            );
    }
    card.into_any_element()
}

/// The canvas-style composer card: the multiline input on top, the model
/// chip and the send circle in the toolbar row — the new-chat canvas in
/// miniature.
pub(super) fn setup_composer(
    page: &mut ProvidersPage,
    theme: &Theme,
    input: Entity<ComposerInput>,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let empty = input.read(cx).text().trim().is_empty();
    div()
        .rounded(px(16.0))
        .bg(theme.input_glass_bg())
        .border_1()
        .border_color(theme.border)
        .px(px(12.0))
        .pt(px(10.0))
        .pb(px(8.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(input)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1())
                // The chip picks the ASSISTANT's model — the brain researching
                // the catalog — not the model being added. Users kept reading
                // it as the target; the label settles that.
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child("Assistant"),
                )
                .child(setup_model_picker(page, theme, cx))
                .child(
                    div()
                        .id("setup-send")
                        .size(px(28.0))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.text)
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(empty, |el| el.opacity(0.35))
                        .when(!empty, |el| {
                            el.cursor_pointer()
                                .hover(|style| style.opacity(0.85))
                                .on_click(cx.listener(|page, _, _, cx| page.send_setup_message(cx)))
                        })
                        .child(
                            crate::icons::icon(crate::icons::ARROW_UP)
                                .size(px(14.0))
                                .text_color(theme.bg),
                        ),
                ),
        )
        .into_any_element()
}

/// The setup picker's catalog, grouped by provider in catalog order — the
/// rail the menu walks. The catalog is built per configured provider
/// ([`configured_model_catalog`]), so every group here has a stored key.
pub(super) struct ModelGroup {
    id: ProviderId,
    models: Vec<Model>,
}

pub(super) fn setup_model_groups(models: &[Model]) -> Vec<ModelGroup> {
    let mut groups: Vec<ModelGroup> = Vec::new();
    for model in models {
        match groups.iter_mut().find(|group| group.id == model.provider) {
            Some(group) => group.models.push(model.clone()),
            None => groups.push(ModelGroup {
                id: model.provider.clone(),
                models: vec![model.clone()],
            }),
        }
    }
    groups
}

/// A rail tab's label: the provider row's display name when the page's own
/// list carries it, else the raw id.
pub(super) fn provider_label(id: &ProviderId, providers: Option<&Vec<Provider>>) -> String {
    providers
        .and_then(|rows| {
            rows.iter()
                .flat_map(|row| row.variants.iter())
                .find(|variant| variant.id == *id)
                .map(|variant| variant.name.clone())
        })
        .or_else(|| {
            providers.and_then(|rows| {
                rows.iter()
                    .find(|row| row.id == *id)
                    .map(|row| row.name.clone())
            })
        })
        .unwrap_or_else(|| id.0.clone())
}

/// The failure a queue frame surfaces: the queue-level error (storage /
/// checkpoint faults) or the first errored pending item (a turn that could
/// not be admitted — missing key, unresolvable model). `None` while the
/// queue is healthy.
pub(super) fn setup_queue_error(queue: &holt_proto::MessageQueue) -> Option<String> {
    queue
        .error
        .clone()
        .or_else(|| queue.pending.iter().find_map(|item| item.error.clone()))
}

/// The composer card's model chip (the new-chat canvas' pattern): quiet
/// text + chevron, the menu opening ABOVE — the composer sits at the
/// dialog's bottom.
pub(super) fn setup_model_picker(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let catalog: Vec<Model> = page
        .setup_models
        .ready()
        .map(|models| models.to_vec())
        .unwrap_or_default();
    let selected_id = page.setup_selected_model.clone();
    let groups = setup_model_groups(&catalog);
    // The rail's active provider: the last rail pick, else the current
    // selection's provider, else the first group. A one-provider catalog
    // (the tests' and most fresh installs') renders no rail at all.
    let active_provider = page
        .setup_model_provider
        .clone()
        .filter(|id| groups.iter().any(|group| &group.id == id))
        .or_else(|| {
            selected_id
                .as_deref()
                .and_then(|id| id.split_once('/'))
                .map(|(provider, _)| ProviderId(provider.into()))
                .filter(|id| groups.iter().any(|group| &group.id == id))
        })
        .or_else(|| groups.first().map(|group| group.id.clone()));
    let mut rows = Vec::new();
    for (index, model) in groups
        .iter()
        .find(|group| Some(&group.id) == active_provider.as_ref())
        .map(|group| group.models.as_slice())
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        let qualified = model.id.clone();
        let selected = selected_id.as_deref() == Some(model.id.as_str());
        rows.push(
            popover::menu_row(theme, selected, format!("setup-model-option-{index}"))
                .id(SharedString::from(format!("setup-model-option-{index}")))
                .debug_selector(move || format!("setup-model-option-{index}"))
                .on_click(
                    cx.listener(move |page, _, _, cx| page.pick_setup_model(qualified.clone(), cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .child(SharedString::from(model.label.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            model
                                .id
                                .split_once('/')
                                .map(|(_, tail)| tail.to_string())
                                .unwrap_or_else(|| model.id.clone()),
                        )),
                )
                .when(selected, |row| {
                    row.child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(13.0))
                            .text_color(theme.accent),
                    )
                })
                .into_any_element(),
        );
    }
    let list = div()
        .id("setup-model-scroll")
        .flex_1()
        .min_w_0()
        .max_h(px(280.0))
        .overflow_y_scroll()
        .occlude()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(rows)
        .into_any_element();
    // One provider configured: the rail would be a single dead tab — the
    // flat list is the whole menu.
    let menu: AnyElement = if groups.len() > 1 {
        let mut rail = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pr(px(6.0))
            .mr(px(6.0))
            .border_r_1()
            .border_color(theme.border);
        for (index, group) in groups.iter().enumerate() {
            let id = group.id.clone();
            let active = Some(&group.id) == active_provider.as_ref();
            let label = provider_label(&group.id, page.providers.ready());
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("setup-model-rail-{index}")))
                    .debug_selector(move || format!("setup-model-rail-{index}"))
                    .w_full()
                    .px(px(6.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .when(active, |tab| tab.bg(crate::theme::ink(0.06)))
                    .when(!active, |tab| {
                        tab.hover(|style| style.bg(crate::theme::ink(0.03)))
                    })
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.setup_model_provider = Some(id.clone());
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_row()
            .child(rail)
            .child(list)
            .into_any_element()
    } else {
        list
    };
    let menu = popover::popover_card(theme)
        .w(px(460.0))
        .occlude()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_setup_model_menu(cx)))
        .child(menu)
        .into_any_element();
    let selected_label = SharedString::from(
        catalog
            .iter()
            .find(|model| Some(model.id.as_str()) == selected_id.as_deref())
            .map(|model| model.label.clone())
            // A stored default from another variant won't match this
            // catalog — fall back to the bare model id, not the qualified.
            .or_else(|| {
                selected_id
                    .as_deref()
                    .and_then(|id| id.rsplit('/').next())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "Select a model".into()),
    );
    div()
        .id("setup-model-dropdown")
        .debug_selector(|| "setup-model-dropdown".into())
        .relative()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.05)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.setup_model_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| page.toggle_setup_model_menu(cx)))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text)
                .child(selected_label),
        )
        .child(
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(page.setup_model_menu.get(), |trigger, _| {
            trigger.child(popover::anchored_menu_above_end_with_priority(
                "setup-model-menu",
                menu,
                page.setup_model_menu.closing_since(),
                popover::ABOVE_MODAL_PRIORITY,
            ))
        })
        .into_any_element()
}

/// What the panel refresh keys on: the proposal-tool chips and the
/// key-request chips the doc holds. Any change (a new chip, one resolving)
/// re-reads the panel's engine-side state.
pub(super) fn proposal_signature(transcript: &[SessionMessageEntry]) -> (usize, usize, usize) {
    let mut total = 0;
    let mut resolved = 0;
    let mut key_requests = 0;
    for entry in transcript {
        for part in &entry.parts {
            if let MessagePart::Tool {
                call,
                resolved: done,
                ..
            } = part
                && matches!(call, ToolCall::Unknown { name, .. } if name == "model_proposal")
            {
                total += 1;
                resolved += *done as usize;
            }
            if let MessagePart::Tool { call, .. } = part
                && matches!(call, ToolCall::Unknown { name, .. } if name == "request_provider_key")
            {
                key_requests += 1;
            }
        }
    }
    (total, resolved, key_requests)
}

/// The URL's host — the part that decides where a key would be sent, and
/// the only part worth the row's width.
pub(super) fn url_host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty())
        .unwrap_or(url)
}

/// Token counts in picker shorthand: 321000 → "321k", 2000000 → "2M".
pub(super) fn fmt_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        format!("{value}")
    }
}

/// One change row's render data: the subject and the fields that decide
/// whether it is safe to write — the endpoint a key would ride to, the
/// window the record bills against, the cost the ledger books. Engine
/// data only (the `proposal_views` JSON), never the agent's prose.
pub(super) fn change_subject(change: &serde_json::Value) -> (String, Option<String>) {
    let str_field = |key: &str| change[key].as_str().map(str::to_string).unwrap_or_default();
    let action = str_field("action");
    match action.as_str() {
        "upsert_model_record" => {
            let record = &change["record"];
            let mut detail = Vec::new();
            if let Some(url) = record["baseUrl"].as_str() {
                detail.push(url_host(url).to_string());
            }
            if let Some(window) = record["contextWindow"].as_u64() {
                detail.push(format!("ctx {}", fmt_tokens(window)));
            }
            if let Some(max) = record["maxTokens"].as_u64() {
                detail.push(format!("out {}", fmt_tokens(max)));
            }
            if let (Some(input), Some(output)) = (
                record["cost"]["input"].as_f64(),
                record["cost"]["output"].as_f64(),
            ) {
                detail.push(format!("${input}/${output} per M"));
            }
            (
                format!("± {}/{}", str_field("providerId"), str_field("modelId")),
                (!detail.is_empty()).then(|| detail.join(" · ")),
            )
        }
        "upsert_custom_provider" => {
            let provider = &change["provider"];
            let mut detail = Vec::new();
            if let Some(url) = provider["baseUrl"].as_str() {
                detail.push(url_host(url).to_string());
            }
            if let Some(api) = provider["defaultApi"].as_str() {
                detail.push(api.to_string());
            }
            (
                format!("+ provider {}", str_field("providerId")),
                (!detail.is_empty()).then(|| detail.join(" · ")),
            )
        }
        "remove_custom_provider" => (format!("− provider {}", str_field("providerId")), None),
        "remove_model_record" => (
            format!("− {}/{}", str_field("providerId"), str_field("modelId")),
            None,
        ),
        "set_hidden_models" => {
            let ids: Vec<&str> = change["modelIds"]
                .as_array()
                .map(|ids| ids.iter().filter_map(|id| id.as_str()).collect())
                .unwrap_or_default();
            let shown = if ids.len() > 6 {
                format!("{} …", ids[..6].join(", "))
            } else {
                ids.join(", ")
            };
            (
                format!("! {}: hide {}", str_field("providerId"), shown),
                None,
            )
        }
        other => (other.to_string(), None),
    }
}

/// The review panel: one card per pending proposal — the engine-built
/// summary, one structured row per change (the fields that decide whether
/// writing is safe), and Write/Discard on the card. Failures land inline
/// on the card they belong to; a written proposal keeps a terminal card
/// instead of vanishing. Proposals live on the session-scoped chat and die
/// with it, so every stored proposal belongs to this session.
pub(super) fn setup_review_panel(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let accent = theme.accent;
    let applying = page.setup_applying.clone();
    let pending: Vec<serde_json::Value> = page.setup_proposals.clone();
    let applied = page.setup_applied.clone();
    let mut panel = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::field_label(theme, "Pending proposals"));
    if pending.is_empty() && applied.is_empty() {
        panel = panel.child(
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child("None yet — a proposal appears here once the assistant prepares one."),
        );
    }
    for (index, proposal) in applied.iter().enumerate() {
        let summary = proposal["summary"].as_str().unwrap_or_default().to_string();
        panel = panel.child(
            div()
                .id(("setup-applied-card", index))
                .debug_selector(move || format!("setup-applied-card-{index}"))
                .flex()
                .items_center()
                .gap(px(8.0))
                .p(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border.opacity(0.5))
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_muted)
                .child(
                    crate::icons::icon(crate::icons::CHECK)
                        .size(px(13.0))
                        .flex_none()
                        .text_color(accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!("Written — {summary}"))),
                ),
        );
    }
    for (index, proposal) in pending.iter().enumerate() {
        let id = proposal["id"].as_str().unwrap_or_default().to_string();
        let summary = proposal["summary"].as_str().unwrap_or_default().to_string();
        let apply_id = id.clone();
        let discard_id = id.clone();
        let busy = applying.as_deref() == Some(id.as_str());
        let error = page.setup_apply_errors.get(&id).cloned();
        let mut card = div()
            .id(("setup-proposal", index))
            .debug_selector(move || format!("setup-proposal-card-{index}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .bg(theme.accent.opacity(0.07))
            .border_color(theme.accent.opacity(0.45))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .child(SharedString::from(summary)),
                    )
                    .child(
                        action_button(theme)
                            .id(("setup-proposal-apply", index))
                            .debug_selector(move || format!("setup-proposal-apply-{index}"))
                            .when(busy, |button| button.opacity(0.4))
                            .hover(|style| style.bg(theme.accent_strong))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.apply_setup_proposal(apply_id.clone(), cx)
                            }))
                            .bg(theme.accent)
                            .text_color(theme.on_accent)
                            .border_color(theme.accent)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Write"),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("setup-proposal-discard", index))
                            .debug_selector(move || format!("setup-proposal-discard-{index}"))
                            .hover(move |style| {
                                style.bg(danger.opacity(0.10)).text_color(danger_muted)
                            })
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.discard_setup_proposal(discard_id.clone(), cx)
                            }))
                            .child("Discard"),
                    ),
            );
        for (row, change) in proposal["changes"]
            .as_array()
            .map(|changes| changes.as_slice())
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            let (subject, detail) = change_subject(change);
            let selector = format!("setup-change-row-{index}-{row}");
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "setup-proposal-change-{index}-{row}"
                    )))
                    .debug_selector(move || selector.clone())
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .pl(px(8.0))
                    .border_l_2()
                    .border_color(theme.accent.opacity(0.5))
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(11.0))
                            .child(SharedString::from(subject)),
                    )
                    .when_some(detail, |card, detail| {
                        card.child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(10.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(detail)),
                        )
                    }),
            );
        }
        if let Some(error) = error {
            card = card.child(
                div()
                    .id(("setup-proposal-error", index))
                    .debug_selector(move || format!("setup-proposal-error-{index}"))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(danger)
                    .child(SharedString::from(format!(
                        "{error} Ask the assistant to propose again."
                    ))),
            );
        }
        panel = panel.child(card);
    }
    panel.into_any_element()
}

/// The configured catalog for the picker: one ListModels per configured
/// provider variant (the title-settings picker's discovery).
pub(super) async fn configured_model_catalog(engine: &crate::state::EngineHandle) -> Vec<Model> {
    let Ok(value) = engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
    else {
        return Vec::new();
    };
    let Ok(providers) = serde_json::from_value::<Vec<Provider>>(value) else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for provider in &providers {
        for variant in &provider.variants {
            if !variant.configured {
                continue;
            }
            if let Ok(value) = engine
                .client()
                .call(
                    methods::LIST_MODELS,
                    serde_json::json!({ "providerId": variant.id.0 }),
                )
                .await
                && let Ok(mut provider_models) = serde_json::from_value::<Vec<Model>>(value)
            {
                models.append(&mut provider_models);
            }
        }
    }
    models
}

/// The first configured provider's first model — the fallback when no chat
/// carries a config.
pub(super) async fn first_configured_model(
    engine: &crate::state::EngineHandle,
) -> Option<(String, String)> {
    let value = engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .ok()?;
    let providers = serde_json::from_value::<Vec<Provider>>(value).ok()?;
    let provider = providers
        .iter()
        .flat_map(|row| &row.variants)
        .find(|variant| variant.configured)?;
    let value = engine
        .client()
        .call(
            methods::LIST_MODELS,
            serde_json::json!({ "providerId": provider.id.0 }),
        )
        .await
        .ok()?;
    let models = serde_json::from_value::<Vec<Model>>(value).ok()?;
    let first = models.first()?;
    // Catalog ids are already provider-qualified.
    Some((provider.id.0.clone(), first.id.clone()))
}

/// The review panel's current proposals.
pub(super) async fn setup_proposal_rows(
    engine: &crate::state::EngineHandle,
    chat_id: &str,
) -> Vec<serde_json::Value> {
    engine
        .client()
        .call(
            methods::LIST_MODEL_PROPOSALS,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// The pending Key request row (`null` when none): the card's data.
pub(super) async fn setup_key_request_row(
    engine: &crate::state::EngineHandle,
    chat_id: &str,
) -> Option<serde_json::Value> {
    engine
        .client()
        .call(
            methods::GET_PROVIDER_KEY_REQUEST,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .ok()
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
}

// -- The AI tab (V2c) --------------------------------------------------

impl ProvidersPage {
    /// AppState changed: the page re-renders when the session view's
    /// emptiness flips (the placeholder ↔ transcript mount decision lives
    /// here — a mounted Transcript re-renders itself), and the review panel
    /// re-reads when the proposal-tool signature moves.
    pub(super) fn on_setup_state_changed(
        &mut self,
        state: Entity<AppState>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        if self.setup_transcript_view.is_none() {
            return;
        }
        let (empty, signature) = {
            let doc = state.read(cx).sub_transcript(&chat_id);
            (doc.is_empty(), proposal_signature(doc))
        };
        if empty != self.setup_doc_empty {
            self.setup_doc_empty = empty;
            cx.notify();
        }
        if signature != self.setup_proposal_signature {
            self.setup_proposal_signature = signature;
            self.refresh_setup_proposals(cx);
        }
    }

    /// The default setup model: the selected chat's config, normalized to
    /// the provider-qualified id (older stored configs may hold a bare id —
    /// the engine's `wire_model_id` rule). The async preparation falls back
    /// to the first configured provider when no chat carries a config.
    pub(super) fn default_setup_model(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let config = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.config.clone())?;
        let provider = config.provider.0.clone();
        let model = format!(
            "{provider}/{}",
            config.model.rsplit('/').next().unwrap_or(&config.model)
        );
        Some((provider, model))
    }

    /// Prepares the AI tab: starts the session's setup chat, loads the
    /// picker catalog, and pins a real Transcript view to the chat (fed by
    /// AppState's doc watch). Idempotent — an already-prepared tab returns.
    pub(super) fn prepare_setup(&mut self, cx: &mut Context<Self>) {
        if self.setup_input.is_none() {
            let input =
                cx.new(|cx| ComposerInput::new("Which provider or model should be set up?", cx));
            // Enter sends, like every other single-shot field; edits
            // repaint so the send circle dims/lights with the content.
            self.setup_input_events = Some(cx.subscribe(
                &input,
                |page: &mut Self, _, event, cx| match event {
                    crate::composer::ComposerInputEvent::Submitted => page.send_setup_message(cx),
                    crate::composer::ComposerInputEvent::Edited => cx.notify(),
                    _ => {}
                },
            ));
            self.setup_input = Some(input);
        }
        if self.setup_transcript_view.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.setup_chat.is_none() {
            self.setup_models = Loadable::Loading;
        }
        let selected_default = self.default_setup_model(cx);
        // The setup chat remembers the last pick (design-v2 decision 1):
        // a carried-over selection outranks re-inheriting the selected
        // chat's config — re-inheriting on every reopen made the user's
        // pick look like it never stuck.
        let remembered = self.setup_selected_model.clone();
        self.setup_task = Some(cx.spawn(async move |this, cx| {
            // Resolve the model: the last pick, else the selected chat's
            // config, else the first configured provider's first model.
            let candidate = remembered.or_else(|| selected_default.map(|(_, model)| model));
            let (provider, model) = match candidate.map(|model| {
                let provider = model.split_once('/').map(|(p, _)| p.to_string());
                (provider, model)
            }) {
                Some((provider, model)) => (provider.unwrap_or_default(), model),
                None => match first_configured_model(&engine).await {
                    Some(pair) => pair,
                    None => {
                        this.update(cx, |page, cx| {
                            page.setup_models = Loadable::Error(
                                "Configure a provider API key first — the AI tab needs a \
                                 working model."
                                    .into(),
                            );
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                },
            };
            let models = configured_model_catalog(&engine).await;
            // The inherited model can outlive the catalog entry it named —
            // a global reset drops custom models while chat configs keep
            // pointing at them. A model the catalog no longer knows cannot
            // admit a turn, so keep it only when it resolves; otherwise
            // take the catalog's first servable entry.
            let model = if models.iter().any(|row| row.id == model) {
                model
            } else {
                let Some(fallback) = models.first().map(|row| row.id.clone()) else {
                    this.update(cx, |page, cx| {
                        page.setup_models = Loadable::Error(
                            "Configure a provider API key first — the AI tab needs a \
                             working model."
                                .into(),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                };
                fallback
            };
            let provider = model
                .split_once('/')
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or(provider);
            let chat_id = match engine
                .client()
                .call(
                    methods::START_MODEL_SETUP_CHAT,
                    serde_json::json!({ "provider": provider, "model": model }),
                )
                .await
            {
                Ok(value) => value
                    .get("chatId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                Err(_) => None,
            };
            let Some(chat_id) = chat_id else {
                this.update(cx, |page, cx| {
                    page.setup_models = Loadable::Error("Could not create the setup chat".into());
                    cx.notify();
                })
                .ok();
                return;
            };
            let key_request = setup_key_request_row(&engine, &chat_id).await;
            let proposals = setup_proposal_rows(&engine, &chat_id).await;
            this.update(cx, |page, cx| {
                // The dialog closed while preparation was in flight: the
                // fresh chat has no owner — delete it so it never becomes
                // a leftover, and set nothing the next open would inherit.
                if page.add_dialog.is_none() {
                    let engine = page.state.read(cx).engine().cloned();
                    if let Some(engine) = engine {
                        let chat_id = chat_id.clone();
                        cx.spawn(async move |_, _| {
                            let _ = engine
                                .client()
                                .call(
                                    methods::MUTATE,
                                    serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
                                )
                                .await;
                        })
                        .detach();
                    }
                    return;
                }
                page.setup_chat = Some(chat_id.clone());
                // The qualified id is both the display value and the run
                // identity — no re-prefixing.
                page.setup_selected_model = Some(model.clone());
                page.setup_model_provider = model
                    .split_once('/')
                    .map(|(provider, _)| ProviderId(provider.into()));
                page.setup_models = Loadable::Ready(models);
                page.setup_proposals = proposals;
                page.set_setup_key_request(key_request, cx);
                // The chat surface is the real Transcript (full markdown,
                // thinking, tool chips, the working trailer). The chat is
                // fresh per session, so its doc holds only this session —
                // no filtering.
                page.state.update(cx, |state, cx| {
                    state.watch_subagent_doc(chat_id.clone(), cx);
                });
                let state = page.state.clone();
                page.setup_transcript_view =
                    Some(cx.new(|cx| {
                        crate::transcript::Transcript::for_doc(state, chat_id, true, cx)
                    }));
                page.watch_setup_queue(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The setup chat's queue watch: the send RPC returns before admission,
    /// so a turn the engine cannot start never reaches the doc — the queue
    /// frame carries the failure instead. Resubscribes like every other
    /// watch; dropped (cancelled) when the dialog closes.
    pub(super) fn watch_setup_queue(&mut self, cx: &mut Context<Self>) {
        if self.setup_queue_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        self.setup_queue_task = Some(cx.spawn(async move |this, cx| {
            loop {
                if let Ok(mut rx) = engine
                    .client()
                    .subscribe(
                        methods::WATCH_MESSAGE_QUEUE,
                        serde_json::json!({ "chatId": chat_id }),
                    )
                    .await
                {
                    while let Some(value) = rx.recv().await {
                        let Ok(queue) = crate::watch_coordinator::WatchCoordinator::decode::<
                            holt_proto::MessageQueue,
                        >(value) else {
                            break;
                        };
                        if this
                            .update(cx, |page, cx| {
                                page.setup_queue = Some(queue);
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                if this
                    .update(cx, |page, cx| {
                        page.setup_queue = None;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    .timer(crate::watch_coordinator::WatchCoordinator::RETRY_DELAY)
                    .await;
            }
        }));
    }

    pub(super) fn pick_setup_model(&mut self, qualified: String, cx: &mut Context<Self>) {
        self.close_setup_model_menu(cx);
        let Some((provider, _)) = qualified.split_once('/') else {
            return;
        };
        let provider = provider.to_string();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_selected_model = Some(qualified.clone());
        self.setup_model_provider = qualified
            .split_once('/')
            .map(|(provider, _)| ProviderId(provider.into()));
        // The chat is session-scoped, so a mid-session pick rewrites the
        // current chat's config (scope preserved) — it must NOT start a
        // fresh chat, or the running conversation would be orphaned.
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let _ = engine
                .client()
                .call(
                    methods::MUTATE,
                    serde_json::json!({
                        "op": "setChatConfig",
                        "chatId": chat_id,
                        "config": {
                            "provider": provider,
                            "model": qualified,
                            "reasoning": null,
                            "modelOptions": {},
                            "scope": "model-setup",
                        },
                    }),
                )
                .await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
        cx.notify();
    }

    pub(super) fn close_setup_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.setup_model_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.setup_model_menu);
            cx.notify();
        }
    }

    pub(super) fn toggle_setup_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.setup_model_menu.take_press_was_open() || self.setup_model_menu.is_open() {
            self.close_setup_model_menu(cx);
        } else {
            self.setup_model_menu.open(());
            cx.notify();
        }
    }

    /// Sends one message into the setup chat; the engine's queue serializes
    /// turns, so a send while the assistant works simply lines up.
    pub(super) fn send_setup_message(&mut self, cx: &mut Context<Self>) {
        let (Some(chat_id), Some(input)) = (self.setup_chat.clone(), self.setup_input.clone())
        else {
            return;
        };
        let prompt = input.read(cx).text().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let Some(selected) = self.setup_selected_model.clone() else {
            return;
        };
        // `RunRequest.model` is the provider-qualified id (the composer's
        // convention); the provider half alone addresses the credential.
        let Some((provider, _)) = selected.split_once('/') else {
            return;
        };
        let provider = provider.to_string();
        let cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.cwd.clone())
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
        input.update(cx, |input, cx| input.set_text("", cx));
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let message_id = format!(
            "setup-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0)
        );
        // The queue snapshot may hold a previous turn's admission failure;
        // a fresh attended send re-admits past the pause, so drop the stale
        // reason — if this send fails too, the next frame brings it back.
        // An errored head parks forever (the pause protects it) — the retry
        // the strip promises must delete those items BEFORE re-enqueueing,
        // or the queue stays blocked and the failure strip never leaves.
        // Read the snapshot first: the clearing below must not erase the
        // very list the deletes are derived from.
        let errored: Vec<String> = self
            .setup_queue
            .as_ref()
            .map(|queue| {
                queue
                    .pending
                    .iter()
                    .filter(|item| item.error.is_some())
                    .map(|item| item.message_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.setup_queue = None;
        // Detached, not slotted: a second send (or any panel action) must
        // not cancel this one mid-flight — the prompt is already cleared,
        // so a cancelled send is a silently lost message.
        cx.spawn(async move |this, cx| {
            for message_id in errored {
                let _ = engine
                    .client()
                    .call(
                        methods::DELETE_QUEUED_MESSAGE,
                        serde_json::json!({ "chatId": chat_id, "messageId": message_id }),
                    )
                    .await;
            }
            let result = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id,
                        "command": {
                            "kind": "run",
                            "messageId": message_id,
                            "request": {
                                "prompt": prompt,
                                "provider": provider,
                                "model": selected,
                                "reasoning": null,
                                "modelOptions": {},
                                "cwd": cwd,
                                "sandbox": "workspace-write",
                            },
                        },
                    }),
                )
                .await;
            if let Err(error) = result {
                this.update(cx, |page, cx| page.fail(error.to_string(), cx))
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// The review panel's write button: applies a stored proposal, then
    /// refreshes everything the page shows. Success remembers the card so
    /// the panel renders a "written" terminal state; failure lands inline
    /// on the card — the usual cause is the apply-time staleness gate, a
    /// property of this proposal, not a page-level fault.
    pub(super) fn apply_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.setup_applying.is_some() {
            return;
        }
        // The card's data: the engine consumes the proposal on success, so
        // the panel keeps its own copy for the terminal render.
        let written = self
            .setup_proposals
            .iter()
            .find(|proposal| proposal["id"] == proposal_id.as_str())
            .cloned();
        self.setup_applying = Some(proposal_id.clone());
        self.setup_apply_errors.remove(&proposal_id);
        cx.notify();
        // Detached: a proposal-signature refresh (or any panel action) must
        // not cancel a Write in flight — the user's click would be lost
        // silently.
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::APPLY_MODEL_PROPOSAL,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id.clone() }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.setup_applying = None;
                match result {
                    Ok(_) => {
                        if let Some(proposal) = written {
                            page.setup_applied.push(proposal);
                        }
                        page.setup_apply_errors.remove(&proposal_id);
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        page.refresh_setup_proposals(cx);
                        // The write lands outside the panel's own actions, so
                        // the cached models/hidden rows are stale — refresh
                        // every visited variant (the cache only holds those).
                        let refresh: Vec<String> = page.models.keys().cloned().collect();
                        for provider in refresh {
                            page.load_models(&provider, true, cx);
                            page.load_hidden(&provider, true, cx);
                        }
                    }
                    Err(error) => {
                        page.setup_apply_errors
                            .insert(proposal_id, error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn discard_setup_proposal(&mut self, proposal_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let _ = engine
                .client()
                .call(
                    methods::DISCARD_MODEL_PROPOSAL,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id }),
                )
                .await;
            this.update(cx, |page, cx| page.refresh_setup_proposals(cx))
                .ok();
        }));
    }

    pub(super) fn refresh_setup_proposals(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.setup_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_panel_task = Some(cx.spawn(async move |this, cx| {
            let proposals = setup_proposal_rows(&engine, &chat_id).await;
            let key_request = setup_key_request_row(&engine, &chat_id).await;
            this.update(cx, |page, cx| {
                page.setup_proposals = proposals;
                page.set_setup_key_request(key_request, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Installs a fetched Key request row (or clears the card when none):
    /// the secret input mounts with the first row and its draft resets
    /// when the ask moves to a different provider, so a stale key never
    /// rides a new destination.
    pub(super) fn set_setup_key_request(
        &mut self,
        row: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        // The ask moves when the provider OR its destination changes — a
        // draft typed against one URL must never ride another.
        let ask_moved = match (&self.setup_key_request, &row) {
            (Some(current), Some(next)) => {
                current["providerId"] != next["providerId"]
                    || current["destination"] != next["destination"]
            }
            _ => false,
        };
        if row.is_some() && self.setup_key_input.is_none() {
            self.setup_key_input = Some(cx.new(|cx| ComposerInput::new_secret("API key", cx)));
        }
        if ask_moved && let Some(input) = &self.setup_key_input {
            input.update(cx, |input, cx| input.set_text("", cx));
        }
        if row.is_none() {
            self.setup_key_input = None;
        }
        self.setup_key_request = row;
    }

    /// The card's settle (ADR-0031): Save carries the typed key straight
    /// to the engine's credential path; Dismiss settles without one. Both
    /// clear the card on success — the engine queues the notice that
    /// continues the setup chat.
    pub(super) fn settle_setup_key_request(&mut self, save: bool, cx: &mut Context<Self>) {
        let (Some(chat_id), Some(_request)) =
            (self.setup_chat.clone(), self.setup_key_request.clone())
        else {
            return;
        };
        if self.setup_key_settling {
            return;
        }
        let key = if save {
            let Some(input) = self.setup_key_input.clone() else {
                return;
            };
            let key = input.read(cx).text().trim().to_string();
            if key.is_empty() {
                return;
            }
            key
        } else {
            String::new()
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.setup_key_settling = true;
        self.setup_key_error = None;
        cx.notify();
        // Detached: a later panel action must not cancel the settle
        // mid-flight — the click would be silently lost and the card
        // stays until a re-request.
        cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "chatId": chat_id });
            if save {
                params["key"] = serde_json::json!(key);
            }
            let result = engine
                .client()
                .call(methods::SETTLE_PROVIDER_KEY_REQUEST, params)
                .await;
            this.update(cx, |page, cx| {
                page.setup_key_settling = false;
                match result {
                    Ok(_) => {
                        page.set_setup_key_request(None, cx);
                        page.setup_key_error = None;
                        // A stored key flips the provider's configured
                        // badge on the panels behind this dialog.
                        if save {
                            page.load(cx);
                        }
                    }
                    Err(error) => page.setup_key_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::providers::test_support::*;

    fn proposal_part(resolved: bool) -> MessagePart {
        MessagePart::Tool {
            id: "t".into(),
            call: ToolCall::Unknown {
                name: "model_proposal".into(),
                input: None,
            },
            is_error: false,
            resolved,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            subagent_usage: None,
            gate: None,
        }
    }

    fn entry_with(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: holt_doc::MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn proposal_signature_counts_only_panel_parts() {
        assert_eq!(proposal_signature(&[]), (0, 0, 0));
        let transcript = vec![
            entry_with("m1", vec![proposal_part(false)]),
            entry_with("m2", vec![proposal_part(true), proposal_part(true)]),
        ];
        assert_eq!(proposal_signature(&transcript), (3, 2, 0));
        // A key-request chip moves the signature too (the card must
        // appear), and other tools never do (no panel refresh storm).
        let mut key_request = proposal_part(true);
        if let MessagePart::Tool { call, .. } = &mut key_request {
            *call = ToolCall::Unknown {
                name: "request_provider_key".into(),
                input: None,
            };
        }
        assert_eq!(
            proposal_signature(&[entry_with("m3", vec![key_request])]),
            (0, 0, 1)
        );
        let mut other = proposal_part(true);
        if let MessagePart::Tool { call, .. } = &mut other {
            *call = ToolCall::WebSearch { query: "x".into() };
        }
        assert_eq!(
            proposal_signature(&[entry_with("m4", vec![other])]),
            (0, 0, 0)
        );
    }

    /// Issue 03's repro: open the AI tab, send, and the placeholder must
    /// flip to the real transcript with the sent turn visible.
    #[gpui::test]
    fn the_setup_dialog_renders_the_sent_turn(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let chat_id = harness.page.update(&mut *harness.visual, |page, _| {
            assert!(page.setup_transcript_view.is_some(), "the view mounts");
            page.setup_chat.clone().expect("the setup chat resolved")
        });
        // A fresh session: the doc starts empty, the placeholder shows.
        assert!(harness.visual.debug_bounds("setup-empty-state").is_some());
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_none(),
            "a healthy queue renders no failure strip"
        );

        // Send like the composer does: draft + submit.
        harness.page.update(&mut *harness.visual, |page, cx| {
            let input = page.setup_input.clone().expect("the composer input");
            input.update(cx, |input, cx| input.set_text("add acme-2", cx));
            page.send_setup_message(cx);
        });
        harness.pump();

        // The turn reached the engine addressed to the setup chat.
        let queued = harness.engine.queued.lock().unwrap().clone();
        assert_eq!(queued.len(), 1, "one QueueCommand");
        assert_eq!(queued[0]["chatId"], chat_id);
        let turn_user_id = queued[0]["command"]["messageId"]
            .as_str()
            .expect("the message id")
            .to_string();

        // The session view holds exactly the fresh turn.
        let rows = harness
            .visual
            .read(|cx| harness.state.read(cx).sub_transcript(&chat_id).to_vec());
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec![turn_user_id.as_str(), "setup-turn-assistant"],
            "the pump published the fresh turn"
        );

        // The placeholder unmounted and the view holds real rows.
        assert!(harness.visual.debug_bounds("setup-empty-state").is_none());
        let view_rows = harness.page.update(&mut *harness.visual, |page, cx| {
            page.setup_transcript_view
                .as_ref()
                .expect("the view")
                .read(cx)
                .rows()
                .to_vec()
        });
        assert!(
            view_rows
                .iter()
                .any(|row| row.id.as_ref().starts_with("setup-turn-assistant#p0")),
            "the turn renders as transcript rows: {:?}",
            view_rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The silent-failure hole (issue 03's persistence mechanism): the send
    /// RPC replies Ok, but the turn never admits — no doc entry, so the
    /// placeholder would sit forever. The queue frame's reason must render,
    /// and the retry the strip promises must actually clear the failure:
    /// the errored head is deleted before the re-send, or it would park the
    /// queue (and the strip) forever.
    #[gpui::test]
    fn a_failed_admission_surfaces_and_the_retry_clears_it(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness_with(cx, true);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let send = |harness: &mut SetupHarness, prompt: &str| {
            harness.page.update(&mut *harness.visual, |page, cx| {
                let input = page.setup_input.clone().expect("the composer input");
                input.update(cx, |input, cx| input.set_text(prompt, cx));
                page.send_setup_message(cx);
            });
            harness.pump();
        };

        // First send: the turn never admits — the placeholder is honest,
        // but the failure is no longer silent.
        send(&mut harness, "add deepseek-flash");
        assert!(harness.visual.debug_bounds("setup-empty-state").is_some());
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_some(),
            "the admission failure renders a strip"
        );
        let reason = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_queue
                .as_ref()
                .and_then(setup_queue_error)
                .unwrap_or_default()
        });
        assert_eq!(reason, "provider deepseek is not configured");

        // The retry: the errored head is deleted, the turn admits, the
        // strip is gone and the conversation renders.
        send(&mut harness, "add deepseek-flash");
        assert!(
            harness.visual.debug_bounds("setup-queue-error").is_none(),
            "the retry cleared the failure strip"
        );
        assert!(harness.visual.debug_bounds("setup-empty-state").is_none());
        let chat_id = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone());
        let rows = harness.visual.read(|cx| {
            harness
                .state
                .read(cx)
                .sub_transcript(chat_id.as_deref().expect("the setup chat"))
                .to_vec()
        });
        assert!(!rows.is_empty(), "the retried turn landed in the doc");
    }

    /// The picker's menu must clear the modal's scrim: deferred layers sort
    /// by priority, and the modal occludes at priority 2 — a priority-1
    /// dropdown paints under the scrim and its rows never receive clicks.
    #[gpui::test]
    fn the_model_menu_picks_from_above_the_modal(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-model-dropdown");
        let row = harness
            .visual
            .debug_bounds("setup-model-option-1")
            .expect("the menu row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));
        // Picking starts the menu's exit (the row unmounts when the close
        // animation's reap timer lands — animation timing, not this test's
        // subject).
        let closing = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_model_menu.closing_since().is_some()
        });
        assert!(closing, "the pick closed the menu");
    }

    /// The picker groups the catalog by provider (the composer picker's
    /// rail): one configured aggregator contributes hundreds of models, and
    /// a flat list of them is unusable. The rail switches which provider's
    /// models the menu lists; picking sets the selection from that rail.
    #[gpui::test]
    fn the_model_menu_scopes_to_the_rail_provider(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-model-dropdown");
        // Two configured providers → two rail tabs; the list opens on the
        // selection's provider (acme) with only acme's models.
        assert!(harness.visual.debug_bounds("setup-model-rail-0").is_some());
        assert!(harness.visual.debug_bounds("setup-model-rail-1").is_some());
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-1")
                .is_some()
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-2")
                .is_none(),
            "beta's model is not listed under acme"
        );

        // Switch the rail: beta's models replace acme's in the same menu.
        harness.click("setup-model-rail-1");
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-0")
                .is_some()
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-model-option-1")
                .is_none(),
            "acme's second model left the list"
        );
        let row = harness
            .visual
            .debug_bounds("setup-model-option-0")
            .expect("the beta row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("beta/beta-1"));
    }

    fn chat_with_config(id: &str, model: &str) -> holt_proto::Chat {
        let provider = model.split('/').next().unwrap_or("acme").to_string();
        holt_proto::Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            title_source: Default::default(),
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: Some("/project".into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: Some(holt_proto::ChatConfig {
                provider: holt_proto::ProviderId(provider.into()),
                model: model.into(),
                reasoning: None,
                model_options: Default::default(),
                permission_mode: Default::default(),
                scope: Default::default(),
            }),
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            space_id: Some("space".into()),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
        }
    }

    /// A global reset drops custom models while the selected chat's config
    /// keeps naming one — the dialog must not inherit a model the catalog
    /// no longer knows (the chip would show a ghost and every send would
    /// fail admission).
    #[gpui::test]
    fn a_ghost_default_falls_back_to_the_catalog(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-ghost"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the ghost default falls back to a servable catalog entry"
        );
    }

    /// The last pick must survive a dialog reopen (design-v2 decision 1):
    /// re-inheriting the selected chat's config every open made the pick
    /// look like it never stuck.
    #[gpui::test]
    fn the_last_pick_survives_a_dialog_reopen(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-custom"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        // The first open inherits the selected chat's model.
        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));

        // Pick the OTHER catalog entry, close, reopen.
        harness.click("setup-model-dropdown");
        let row = harness
            .visual
            .debug_bounds("setup-model-option-0")
            .expect("the menu row renders");
        harness
            .visual
            .simulate_click(row.center(), Default::default());
        harness.pump();
        harness.click("add-provider-close");
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the reopen keeps the pick instead of re-inheriting the chat config"
        );
    }

    /// The full story the user hit (2026-09-18): the selected chat runs a
    /// custom model, the dialog inherits it, a global reset drops the
    /// custom entry — the reopened dialog must fall back to a servable
    /// model instead of pinning the ghost.
    #[gpui::test]
    fn a_reset_custom_model_does_not_ghost_the_reopened_dialog(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness.state.update(&mut *harness.visual, |state, _| {
            state
                .chats
                .push(chat_with_config("chat-1", "acme/acme-custom"));
            state.selected_chat = Some("chat-1".into());
        });
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        // Pre-reset the inherited custom model is a legitimate pick.
        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(picked.as_deref(), Some("acme/acme-custom"));

        // Close, reset globally (the page's two-step confirm), reopen.
        harness.click("add-provider-close");
        harness.click("reset-all-providers");
        harness.click("reset-all-confirm");
        assert_eq!(
            harness.engine.resets.lock().unwrap().len(),
            1,
            "the confirm fires the reset RPC"
        );
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let picked = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_selected_model.clone()
        });
        assert_eq!(
            picked.as_deref(),
            Some("acme/acme-1"),
            "the reset custom model no longer pins the chip"
        );
    }

    /// A stored proposal as the engine's `proposal_views` serves it.
    fn proposal_json(id: &str, created_at: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "summary": "Apply 1 catalog change for acme",
            "createdAt": created_at,
            "changes": [{
                "action": "upsert_model_record",
                "providerId": "acme",
                "modelId": "acme-2",
                "record": {
                    "id": "acme-2",
                    "name": "Acme 2",
                    "api": "openai-completions",
                    "provider": "acme",
                    "baseUrl": "https://acme.example/v1",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": { "input": 1.5, "output": 3.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
                    "contextWindow": 200_000,
                    "maxTokens": 8_192,
                },
            }],
        })
    }

    /// The write decision shows its own data: each pending card carries the
    /// structured change rows (endpoint, window, cost) beside the summary,
    /// and a successful Write lands as a terminal "written" card instead
    /// of silently vanishing.
    #[gpui::test]
    fn the_review_panel_renders_change_rows_and_writes(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .proposals
            .lock()
            .unwrap()
            .push(proposal_json("prop-1", now_ms() + 30_000));
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        let (card, row) = (
            harness.visual.debug_bounds("setup-proposal-card-0"),
            harness.visual.debug_bounds("setup-change-row-0-0"),
        );
        assert!(card.is_some(), "the proposal card renders");
        assert!(row.is_some(), "the change's structured row renders");

        harness.click("setup-proposal-apply-0");
        assert_eq!(harness.engine.applies.lock().unwrap().len(), 1);
        assert!(
            harness
                .visual
                .debug_bounds("setup-applied-card-0")
                .is_some(),
            "the write lands as a visible terminal card"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-card-0")
                .is_none(),
            "the written proposal stops offering Write"
        );
    }

    fn seed_key_request(engine: &FakeSetupEngine) {
        *engine.key_request.lock().unwrap() = Some(serde_json::json!({
            "providerId": "beta",
            "providerName": "Beta Labs",
            "destination": "https://api.beta.example/v1",
            "hasKey": false,
        }));
    }

    /// The Key request's happy path (issue 01): the card renders while a
    /// request is pending, Save carries the typed value to the settle RPC
    /// (never through the chat), and the card clears.
    #[gpui::test]
    fn the_key_request_card_saves_the_typed_key(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        seed_key_request(&harness.engine);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-card")
                .is_some(),
            "the card renders while a request is pending"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-destination")
                .is_some(),
            "the destination the key rides to is shown"
        );

        harness.page.update(&mut *harness.visual, |page, cx| {
            let input = page.setup_key_input.clone().expect("the card's input");
            input.update(cx, |input, cx| input.set_text("sk-ui-secret", cx));
        });
        harness.click("setup-key-request-save");
        harness.pump();

        let settles = harness.engine.settles.lock().unwrap().clone();
        assert_eq!(settles.len(), 1, "one settle call");
        assert_eq!(settles[0]["key"], "sk-ui-secret");
        assert_eq!(settles[0]["chatId"], "setup-chat-0");
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-card")
                .is_none(),
            "the card clears after the save"
        );
    }

    /// Dismiss settles without a key and without touching credentials; the
    /// card shows the stored-key hint when the provider already has one.
    #[gpui::test]
    fn the_key_request_card_dismisses_without_a_key(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        seed_key_request(&harness.engine);
        if let Some(row) = harness.engine.key_request.lock().unwrap().as_mut() {
            row["hasKey"] = serde_json::json!(true);
        }
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-existing")
                .is_some(),
            "the stored-key hint renders"
        );

        harness.click("setup-key-request-dismiss");
        harness.pump();

        let settles = harness.engine.settles.lock().unwrap().clone();
        assert_eq!(settles.len(), 1, "one settle call");
        assert!(
            settles[0].get("key").is_none(),
            "the dismissal carries no key"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-card")
                .is_none(),
            "the card clears after the dismissal"
        );
    }

    /// An empty input never sends a settle with an empty key — the Save
    /// button is dead until something is typed.
    #[gpui::test]
    fn an_empty_key_input_does_not_settle(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        seed_key_request(&harness.engine);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.page.update(&mut *harness.visual, |page, cx| {
            page.settle_setup_key_request(true, cx);
        });
        harness.pump();

        assert!(
            harness.engine.settles.lock().unwrap().is_empty(),
            "no settle without a typed key"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-card")
                .is_some(),
            "the card stays"
        );
    }

    /// An apply failure is a property of the proposal (usually the
    /// staleness gate), not a page fault: it renders inline on the card it
    /// belongs to, with the Write still available for a retry after the
    /// assistant re-proposes — never as the window-top modal.
    #[gpui::test]
    fn an_apply_failure_lands_inline_on_the_card(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .fail_applies
            .store(true, std::sync::atomic::Ordering::SeqCst);
        harness
            .engine
            .proposals
            .lock()
            .unwrap()
            .push(proposal_json("prop-1", now_ms() + 30_000));
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        harness.click("setup-proposal-apply-0");
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-error-0")
                .is_some(),
            "the failure renders on the card"
        );
        assert!(
            harness
                .visual
                .debug_bounds("setup-proposal-card-0")
                .is_some(),
            "the card stays for a retry"
        );
        let inline = harness.page.update(&mut *harness.visual, |page, _| {
            page.setup_apply_errors.get("prop-1").cloned()
        });
        assert!(
            inline
                .unwrap_or_default()
                .contains("changed since this proposal"),
            "the inline error carries the engine's reason"
        );
    }

    /// The setup chat is session-scoped: closing the dialog deletes it
    /// (turn cancelled, transcript and proposals dropped with it), and the
    /// next open starts a fresh chat — no conversation memory persists.
    #[gpui::test]
    fn closing_the_dialog_deletes_the_setup_chat(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        seed_key_request(&harness.engine);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        let first = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone())
            .expect("the setup chat resolved");
        assert!(
            harness
                .visual
                .debug_bounds("setup-key-request-card")
                .is_some(),
            "the seeded key request renders"
        );

        harness.click("add-provider-close");
        let (request, input) = harness.page.update(&mut *harness.visual, |page, _| {
            (
                page.setup_key_request.is_some(),
                page.setup_key_input.is_some(),
            )
        });
        assert!(
            !request && !input,
            "the pending key request dies with the dialog"
        );
        assert_eq!(
            harness.engine.deleted_chats.lock().unwrap().as_slice(),
            &[first.clone()],
            "closing the dialog deletes the session's chat"
        );

        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();
        let second = harness
            .page
            .update(&mut *harness.visual, |page, _| page.setup_chat.clone())
            .expect("the setup chat resolved");
        assert_ne!(first, second, "the reopen starts a fresh chat");
    }
}
