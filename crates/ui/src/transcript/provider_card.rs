//! The Provider Mode cards (ADR-0037): a catalog proposal and an API-key
//! request, rendered in the transcript flow where the assistant raised
//! them. Only these buttons write — Write applies the stored proposal
//! exactly as reviewed, Save sends the typed key straight to the
//! credential store. The key lives only in the masked input entity (keyed
//! by row id, dropped on settle); the row and the doc never carry it.
//! Settled states come back from the engine as doc stamps, so the cards
//! hold no outcome of their own — only in-flight and error state.

use gpui::{AnyElement, Entity, SharedString, Subscription, div, prelude::*, px};

use holt_doc::{ChoiceCardState, KeyCardState, ProposalCardState, ProviderRef};
use holt_rpc::methods;

use super::{Transcript, TranscriptEvent};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::settings::widgets;
use crate::theme::Theme;

/// Per-card interactive state, keyed by row id on the [`Transcript`].
#[derive(Default)]
pub(super) struct ProviderCardUi {
    busy: bool,
    error: Option<SharedString>,
    key_input: Option<Entity<ComposerInput>>,
    _key_input_events: Option<Subscription>,
}

/// The apply-time staleness gate's refusal reads as an instruction, not a
/// fault: the assistant re-proposes against the current catalog.
fn proposal_error_text(error: &str) -> String {
    if error.contains("changed since this proposal") {
        "Catalog changed — ask to re-propose.".to_string()
    } else {
        error.to_string()
    }
}

fn primary_button(theme: &Theme) -> gpui::Div {
    div()
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .bg(theme.accent)
        .text_color(theme.on_accent)
        .text_size(crate::typography::ui_rems(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
}

fn card_frame(theme: &Theme, active: bool) -> gpui::Div {
    div()
        .w_full()
        .max_w(px(720.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .when(active, |card| {
            card.bg(theme.accent.opacity(0.07))
                .border_color(theme.accent.opacity(0.45))
        })
        .when(!active, |card| card.border_color(theme.hairline(0.12)))
}

fn state_line(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// A provider's brand mark, or the generic bot glyph.
fn provider_mark(provider_id: &str, theme: &Theme) -> gpui::Svg {
    let (path, tint) =
        crate::pickers::provider_brand_icon(&holt_proto::ProviderId::from(provider_id))
            .unwrap_or((crate::icons::BOT, Some(theme.text_muted)));
    crate::icons::icon(path)
        .size(px(14.0))
        .flex_none()
        .text_color(tint.unwrap_or(theme.text))
}

/// The proposal's target providers: mark, name, and the concrete id — an
/// organization's variants differ only there.
fn target_header(targets: &[ProviderRef], theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(12.0))
        .gap_y(px(4.0))
        .children(targets.iter().map(|target| {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(provider_mark(&target.id, theme))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(target.name.clone())),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(target.id.clone())),
                )
        }))
}

impl Transcript {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_model_proposal_card(
        &mut self,
        row_id: &SharedString,
        proposal_id: &SharedString,
        targets: &[ProviderRef],
        summary: &SharedString,
        lines: &[SharedString],
        state: ProposalCardState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = state == ProposalCardState::Pending;
        let ui = self.provider_cards.get(row_id);
        let busy = ui.is_some_and(|ui| ui.busy);
        let error = ui.and_then(|ui| ui.error.clone());
        let mut card = card_frame(theme, pending)
            .when(!targets.is_empty(), |card| {
                card.child(target_header(targets, theme))
            })
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text)
                    .child(summary.clone()),
            );
        if !lines.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .pl(px(8.0))
                    .border_l_2()
                    .border_color(theme.accent.opacity(if pending { 0.5 } else { 0.2 }))
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(16.0))
                    .text_color(theme.text_muted)
                    .children(lines.iter().cloned()),
            );
        }
        card = match state {
            ProposalCardState::Pending => {
                let write_row = row_id.clone();
                let write_id = proposal_id.clone();
                let discard_row = row_id.clone();
                let discard_id = proposal_id.clone();
                let danger = theme.danger;
                let danger_muted = theme.danger_muted;
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .when(busy, |row| row.opacity(0.4))
                        .child(
                            primary_button(theme)
                                .id(SharedString::from(format!(
                                    "provider-proposal-write-{row_id}"
                                )))
                                .debug_selector({
                                    let row_id = row_id.clone();
                                    move || format!("provider-proposal-write-{row_id}")
                                })
                                .hover(|style| style.bg(theme.accent_strong))
                                .when(!busy, |button| {
                                    button.on_click(cx.listener(move |this, _, _, cx| {
                                        this.settle_proposal(
                                            write_row.clone(),
                                            write_id.clone(),
                                            true,
                                            cx,
                                        )
                                    }))
                                })
                                .child("Write"),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id(SharedString::from(format!(
                                    "provider-proposal-discard-{row_id}"
                                )))
                                .debug_selector({
                                    let row_id = row_id.clone();
                                    move || format!("provider-proposal-discard-{row_id}")
                                })
                                .hover(move |style| {
                                    style.bg(danger.opacity(0.10)).text_color(danger_muted)
                                })
                                .when(!busy, |button| {
                                    button.on_click(cx.listener(move |this, _, _, cx| {
                                        this.settle_proposal(
                                            discard_row.clone(),
                                            discard_id.clone(),
                                            false,
                                            cx,
                                        )
                                    }))
                                })
                                .child("Discard"),
                        ),
                )
            }
            ProposalCardState::Written => match targets.first() {
                None => card.child(state_line("✓ Written", theme)),
                Some(first) => {
                    let names = targets
                        .iter()
                        .map(|target| target.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let provider_id = first.id.clone();
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(state_line(format!("✓ Written to {names}"), theme))
                            .child(
                                widgets::ghost_action(theme)
                                    .id(SharedString::from(format!(
                                        "provider-proposal-settings-{row_id}"
                                    )))
                                    .debug_selector({
                                        let row_id = row_id.clone();
                                        move || format!("provider-proposal-settings-{row_id}")
                                    })
                                    .py(px(2.0))
                                    .hover(move |style| widgets::ghost_hover(theme, style))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(TranscriptEvent::OpenProviderSettings {
                                            provider_id: provider_id.clone(),
                                        })
                                    }))
                                    .child("Open in Settings"),
                            ),
                    )
                }
            },
            ProposalCardState::Discarded => card.child(state_line("Discarded", theme)),
            ProposalCardState::Superseded => {
                card.child(state_line("Superseded by a newer proposal", theme))
            }
        };
        div()
            .py(px(4.0))
            .w_full()
            .child(card.children(error.map(|error| {
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.danger)
                    .child(error)
            })))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_provider_choice_card(
        &mut self,
        row_id: &SharedString,
        card_id: &SharedString,
        options: &[ProviderRef],
        chosen: Option<&SharedString>,
        state: ChoiceCardState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = state == ChoiceCardState::Pending;
        let ui = self.provider_cards.get(row_id);
        let busy = ui.is_some_and(|ui| ui.busy);
        let error = ui.and_then(|ui| ui.error.clone());
        let card = card_frame(theme, pending).child(
            div()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text)
                .child("Which provider?"),
        );
        let card = match state {
            ChoiceCardState::Pending => card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .when(busy, |list| list.opacity(0.4))
                    .children(options.iter().map(|option| {
                        let selector = format!("provider-choice-{row_id}-{}", option.id);
                        let settle_row = row_id.clone();
                        let settle_card = card_id.clone();
                        let provider_id = SharedString::from(option.id.clone());
                        let detail = if option.detail.is_empty() {
                            option.id.clone()
                        } else {
                            format!("{} · {}", option.id, option.detail)
                        };
                        div()
                            .id(SharedString::from(selector.clone()))
                            .debug_selector(move || selector.clone())
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .py(px(6.0))
                            .rounded(px(Theme::CONTROL_RADIUS))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.hairline(0.06)))
                            .child(provider_mark(&option.id, theme))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(12.5))
                                            .text_color(theme.text)
                                            .child(SharedString::from(option.name.clone())),
                                    )
                                    .child(
                                        div()
                                            .font_family(theme.font_mono.clone())
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(theme.text_muted)
                                            .truncate()
                                            .child(SharedString::from(detail)),
                                    ),
                            )
                            .when(option.configured, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .px(px(6.0))
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(theme.hairline(0.12))
                                        .text_size(crate::typography::ui_rems(10.5))
                                        .text_color(theme.text_muted)
                                        .child("Configured"),
                                )
                            })
                            .when(!busy, |row| {
                                row.on_click(cx.listener(move |this, _, _, cx| {
                                    this.settle_choice(
                                        settle_row.clone(),
                                        settle_card.clone(),
                                        provider_id.clone(),
                                        cx,
                                    )
                                }))
                            })
                    })),
            ),
            ChoiceCardState::Chosen => {
                let chosen = chosen.map(SharedString::as_ref).unwrap_or_default();
                let name = options
                    .iter()
                    .find(|option| option.id == chosen)
                    .map_or(chosen, |option| option.name.as_str());
                card.child(state_line(format!("✓ Using {name} · {chosen}"), theme))
            }
            ChoiceCardState::Superseded => card.child(state_line("No longer active", theme)),
        };
        div()
            .py(px(4.0))
            .w_full()
            .child(card.children(error.map(|error| {
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.danger)
                    .child(error)
            })))
            .into_any_element()
    }

    pub(super) fn render_key_request_card(
        &mut self,
        row_id: &SharedString,
        provider_name: &SharedString,
        destination: &SharedString,
        state: KeyCardState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = state == KeyCardState::Pending;
        // Settled cards lose their input in `sync`, rendered or not.
        if pending
            && self
                .provider_cards
                .get(row_id)
                .is_none_or(|ui| ui.key_input.is_none())
        {
            let input = cx.new(|cx| ComposerInput::new_secret("API key", cx));
            let submit_row = row_id.clone();
            let events = cx.subscribe(&input, move |this, _, event, cx| match event {
                ComposerInputEvent::Edited => cx.notify(),
                ComposerInputEvent::Submitted => this.settle_key(submit_row.clone(), true, cx),
                _ => {}
            });
            let ui = self.provider_cards.entry(row_id.clone()).or_default();
            ui.key_input = Some(input);
            ui._key_input_events = Some(events);
        }
        let ui = self.provider_cards.get(row_id);
        let busy = ui.is_some_and(|ui| ui.busy);
        let error = ui.and_then(|ui| ui.error.clone());
        let input = ui.and_then(|ui| ui.key_input.clone());
        let typed = input
            .as_ref()
            .is_some_and(|input| !input.read(cx).text().trim().is_empty());
        let mut card = card_frame(theme, pending).child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(theme.text)
                        .child(SharedString::from(format!("API key for {provider_name}"))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted)
                        .child(destination.clone()),
                ),
        );
        card = match state {
            KeyCardState::Pending => {
                let save_row = row_id.clone();
                let dismiss_row = row_id.clone();
                let enabled = typed && !busy;
                card.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.8))
                        .child(
                            "Saved locally and sent only to this destination — never to the chat.",
                        ),
                )
                .children(input.map(|input| {
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.input_glass_bg())
                        .child(input)
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            primary_button(theme)
                                .id(SharedString::from(format!("provider-key-save-{row_id}")))
                                .debug_selector({
                                    let row_id = row_id.clone();
                                    move || format!("provider-key-save-{row_id}")
                                })
                                .hover(|style| style.bg(theme.accent_strong))
                                .when(!enabled, |button| button.opacity(0.35))
                                .when(enabled, |button| {
                                    button.on_click(cx.listener(move |this, _, _, cx| {
                                        this.settle_key(save_row.clone(), true, cx)
                                    }))
                                })
                                .child("Save key"),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id(SharedString::from(format!("provider-key-dismiss-{row_id}")))
                                .debug_selector({
                                    let row_id = row_id.clone();
                                    move || format!("provider-key-dismiss-{row_id}")
                                })
                                .hover(move |style| widgets::ghost_hover(theme, style))
                                .when(!busy, |button| {
                                    button.on_click(cx.listener(move |this, _, _, cx| {
                                        this.settle_key(dismiss_row.clone(), false, cx)
                                    }))
                                })
                                .child("Dismiss"),
                        ),
                )
            }
            KeyCardState::Saved => card.child(state_line("✓ Key saved", theme)),
            KeyCardState::Dismissed => card.child(state_line("Dismissed", theme)),
            KeyCardState::Superseded => {
                card.child(state_line("Superseded by a newer request", theme))
            }
        };
        div()
            .py(px(4.0))
            .w_full()
            .child(card.children(error.map(|error| {
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.danger)
                    .child(error)
            })))
            .into_any_element()
    }

    /// Write (`apply`) or Discard a stored proposal. The engine stamps the
    /// card on success; a failure stays under the card, which stays
    /// pending.
    fn settle_proposal(
        &mut self,
        row_id: SharedString,
        proposal_id: SharedString,
        apply: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(chat_id), Some(engine)) =
            (self.chat_id.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        let ui = self.provider_cards.entry(row_id.clone()).or_default();
        if ui.busy {
            return;
        }
        ui.busy = true;
        ui.error = None;
        self.remeasure_row(&row_id);
        cx.notify();
        let method = if apply {
            methods::APPLY_MODEL_PROPOSAL
        } else {
            methods::DISCARD_MODEL_PROPOSAL
        };
        // Detached: a transcript re-sync must not cancel a Write in flight.
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    method,
                    serde_json::json!({ "chatId": chat_id, "proposalId": proposal_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = this.provider_cards.get_mut(&row_id) {
                    ui.busy = false;
                    ui.error = result
                        .as_ref()
                        .err()
                        .map(|error| proposal_error_text(&error.to_string()).into());
                }
                if apply && result.is_ok() {
                    crate::pickers::bump_provider_catalog(cx);
                }
                this.remeasure_row(&row_id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Settle a provider choice card on one of its options. The engine
    /// stamps the card and queues "Use provider <id>" as the next message.
    fn settle_choice(
        &mut self,
        row_id: SharedString,
        card_id: SharedString,
        provider_id: SharedString,
        cx: &mut Context<Self>,
    ) {
        let (Some(chat_id), Some(engine)) =
            (self.chat_id.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        let ui = self.provider_cards.entry(row_id.clone()).or_default();
        if ui.busy {
            return;
        }
        ui.busy = true;
        ui.error = None;
        self.remeasure_row(&row_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SETTLE_PROVIDER_CHOICE,
                    serde_json::json!({
                        "chatId": chat_id,
                        "cardId": card_id,
                        "providerId": provider_id,
                    }),
                )
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = this.provider_cards.get_mut(&row_id) {
                    ui.busy = false;
                    ui.error = result.as_ref().err().map(|error| error.to_string().into());
                }
                this.remeasure_row(&row_id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Save (with the typed key) or Dismiss the chat's pending key request
    /// (ADR-0031). The key goes from the input straight into the RPC.
    fn settle_key(&mut self, row_id: SharedString, save: bool, cx: &mut Context<Self>) {
        let (Some(chat_id), Some(engine)) =
            (self.chat_id.clone(), self.state.read(cx).engine().cloned())
        else {
            return;
        };
        let Some(ui) = self.provider_cards.get_mut(&row_id) else {
            return;
        };
        if ui.busy {
            return;
        }
        let mut params = serde_json::json!({ "chatId": chat_id });
        if save {
            let key = ui
                .key_input
                .as_ref()
                .map(|input| input.read(cx).text().trim().to_string())
                .unwrap_or_default();
            if key.is_empty() {
                return;
            }
            params["key"] = serde_json::Value::String(key);
        }
        ui.busy = true;
        ui.error = None;
        self.remeasure_row(&row_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SETTLE_PROVIDER_KEY_REQUEST, params)
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = this.provider_cards.get_mut(&row_id) {
                    ui.busy = false;
                    ui.error = result.as_ref().err().map(|error| error.to_string().into());
                }
                // A stored key can make a provider's models listable.
                if save && result.is_ok() {
                    crate::pickers::bump_provider_catalog(cx);
                }
                this.remeasure_row(&row_id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::transcript::model::RowKind;
    use holt_doc::{MessagePart, MessageRole, SessionMessageEntry};

    fn entry(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn proposal_part(state: ProposalCardState) -> MessagePart {
        MessagePart::ModelProposal {
            id: "p1".into(),
            proposal_id: "prop-1".into(),
            targets: vec![],
            summary: "Add acme/acme-2".into(),
            lines: vec!["+ acme/acme-2  200k ctx".into()],
            state,
        }
    }

    fn provider_ref(id: &str, name: &str) -> ProviderRef {
        ProviderRef {
            id: id.into(),
            name: name.into(),
            detail: format!("api.{id}.example"),
            configured: false,
        }
    }

    fn written_part() -> MessagePart {
        MessagePart::ModelProposal {
            id: "p1".into(),
            proposal_id: "prop-1".into(),
            targets: vec![provider_ref("xiaomi-cn", "Xiaomi CN")],
            summary: "Add xiaomi-cn/mimo".into(),
            lines: vec![],
            state: ProposalCardState::Written,
        }
    }

    fn choice_part(state: ChoiceCardState, chosen: Option<&str>) -> MessagePart {
        MessagePart::ProviderChoice {
            id: "c1".into(),
            options: vec![
                provider_ref("xiaomi", "Xiaomi"),
                provider_ref("xiaomi-cn", "Xiaomi CN"),
            ],
            chosen: chosen.map(str::to_owned),
            state,
        }
    }

    fn key_part(state: KeyCardState) -> MessagePart {
        MessagePart::KeyRequest {
            id: "k1".into(),
            provider_id: "beta".into(),
            provider_name: String::new(),
            destination: "https://api.beta.example/v1".into(),
            state,
        }
    }

    #[test]
    fn provider_mode_parts_become_card_rows() {
        use crate::markdown::parser::{BlockTree, parse_full};

        let mut parse = |_: &str, text: &str| Arc::new(parse_full(text)) as Arc<BlockTree>;
        let rows = crate::transcript::rows_for_entry(
            &entry(
                "a1",
                vec![
                    proposal_part(ProposalCardState::Pending),
                    key_part(KeyCardState::Pending),
                ],
            ),
            false,
            &mut parse,
        );
        assert_eq!(rows.len(), 2);
        let RowKind::ModelProposal {
            proposal_id,
            summary,
            lines,
            ..
        } = &rows[0].kind
        else {
            panic!("expected the proposal row");
        };
        assert_eq!(proposal_id.as_ref(), "prop-1");
        assert_eq!(summary.as_ref(), "Add acme/acme-2");
        assert_eq!(lines.len(), 1);
        let RowKind::KeyRequest {
            provider_name,
            destination,
            ..
        } = &rows[1].kind
        else {
            panic!("expected the key request row");
        };
        // An unnamed provider falls back to its id.
        assert_eq!(provider_name.as_ref(), "beta");
        assert_eq!(destination.as_ref(), "https://api.beta.example/v1");

        let rows_of = |parts| {
            crate::transcript::rows_for_entry(
                &entry("a2", parts),
                false,
                &mut |_: &str, text: &str| Arc::new(parse_full(text)) as Arc<BlockTree>,
            )
        };
        let pending = rows_of(vec![choice_part(ChoiceCardState::Pending, None)]);
        let RowKind::ProviderChoice {
            card_id, options, ..
        } = &pending[0].kind
        else {
            panic!("expected the provider choice row");
        };
        assert_eq!(card_id.as_ref(), "c1");
        assert_eq!(options.len(), 2);
        let chosen = rows_of(vec![choice_part(
            ChoiceCardState::Chosen,
            Some("xiaomi-cn"),
        )]);
        assert_ne!(pending[0].version, chosen[0].version);
        let RowKind::ModelProposal { targets, .. } = &rows_of(vec![written_part()])[0].kind else {
            panic!("expected the proposal row");
        };
        assert_eq!(targets[0].id, "xiaomi-cn");

        // A settle re-versions the row so it re-measures.
        let settled = crate::transcript::rows_for_entry(
            &entry("a1", vec![proposal_part(ProposalCardState::Written)]),
            false,
            &mut parse,
        );
        assert_ne!(rows[0].version, settled[0].version);
    }

    #[test]
    fn the_staleness_refusal_reads_as_an_instruction() {
        assert_eq!(
            proposal_error_text("provider settings changed since this proposal was created"),
            "Catalog changed — ask to re-propose."
        );
        assert_eq!(proposal_error_text("boom"), "boom");
    }

    /// Records the card RPCs.
    struct CardEngine {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait::async_trait]
    impl holt_rpc::RpcService for CardEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
            match method {
                methods::APPLY_MODEL_PROPOSAL
                | methods::DISCARD_MODEL_PROPOSAL
                | methods::SETTLE_PROVIDER_KEY_REQUEST
                | methods::SETTLE_PROVIDER_CHOICE => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push((method.to_string(), params));
                    holt_rpc::RpcReply::value(&serde_json::json!({}))
                }
                _ => Err(holt_rpc::RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    /// The cards' buttons are the only writers: Write applies the stored
    /// proposal by id, Save sends the typed key in the settle call, and a
    /// settled stamp retires the buttons (and the key input).
    #[gpui::test]
    fn provider_cards_write_only_through_their_buttons(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let engine = Arc::new(CardEngine {
            calls: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| AppState::new());
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));
        let pump = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..8 {
                runtime.block_on(async { tokio::task::yield_now().await });
                cx.run_until_parked();
            }
        };

        state.update(cx, |s, cx| {
            // The transcript follows the selection; the cards address it.
            s.selected_chat = Some("chat-1".into());
            s.transcript.push(entry(
                "a1",
                vec![
                    proposal_part(ProposalCardState::Pending),
                    key_part(KeyCardState::Pending),
                ],
            ));
            cx.notify();
        });
        pump(cx);
        assert!(cx.debug_bounds("provider-proposal-discard-a1#p1").is_some());
        assert!(cx.debug_bounds("provider-key-dismiss-a1#k1").is_some());

        let write = cx
            .debug_bounds("provider-proposal-write-a1#p1")
            .expect("the Write button renders");
        cx.simulate_click(write.center(), Default::default());
        pump(cx);

        transcript.update(cx, |this, cx| {
            let input = this.provider_cards["a1#k1"]
                .key_input
                .clone()
                .expect("the pending card owns a key input");
            input.update(cx, |input, cx| input.set_text("sk-beta-secret", cx));
        });
        pump(cx);
        let save = cx
            .debug_bounds("provider-key-save-a1#k1")
            .expect("the Save button renders");
        cx.simulate_click(save.center(), Default::default());
        pump(cx);

        assert_eq!(
            engine.calls.lock().unwrap().as_slice(),
            &[
                (
                    methods::APPLY_MODEL_PROPOSAL.to_string(),
                    serde_json::json!({ "chatId": "chat-1", "proposalId": "prop-1" }),
                ),
                (
                    methods::SETTLE_PROVIDER_KEY_REQUEST.to_string(),
                    serde_json::json!({ "chatId": "chat-1", "key": "sk-beta-secret" }),
                ),
            ]
        );

        // The engine's stamps settle both cards in place.
        state.update(cx, |s, cx| {
            s.transcript[0] = entry(
                "a1",
                vec![
                    proposal_part(ProposalCardState::Written),
                    key_part(KeyCardState::Saved),
                ],
            );
            cx.notify();
        });
        pump(cx);
        assert!(cx.debug_bounds("provider-proposal-write-a1#p1").is_none());
        assert!(cx.debug_bounds("provider-key-save-a1#k1").is_none());
        transcript.update(cx, |this, _| {
            assert!(
                !this.provider_cards.contains_key("a1#k1"),
                "the settled card dropped its key input"
            );
        });
    }

    /// A choice card settles only through a click on one of its options;
    /// the written proposal's Settings link names its target provider.
    #[gpui::test]
    fn a_choice_click_settles_and_a_written_card_links_to_settings(cx: &mut gpui::TestAppContext) {
        use crate::state::AppState;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let engine = Arc::new(CardEngine {
            calls: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| AppState::new());
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (transcript, cx) = cx.add_window_view(|_, cx| Transcript::new(state.clone(), cx));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = events.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&transcript, move |_, event: &TranscriptEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
        });
        let pump = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..8 {
                runtime.block_on(async { tokio::task::yield_now().await });
                cx.run_until_parked();
            }
        };

        state.update(cx, |s, cx| {
            s.selected_chat = Some("chat-1".into());
            s.transcript.push(entry(
                "a1",
                vec![choice_part(ChoiceCardState::Pending, None), written_part()],
            ));
            cx.notify();
        });
        pump(cx);
        let option = cx
            .debug_bounds("provider-choice-a1#c1-xiaomi-cn")
            .expect("the option renders");
        cx.simulate_click(option.center(), Default::default());
        pump(cx);
        assert_eq!(
            engine.calls.lock().unwrap().as_slice(),
            &[(
                methods::SETTLE_PROVIDER_CHOICE.to_string(),
                serde_json::json!({
                    "chatId": "chat-1",
                    "cardId": "c1",
                    "providerId": "xiaomi-cn",
                }),
            )]
        );

        let settings = cx
            .debug_bounds("provider-proposal-settings-a1#p1")
            .expect("the written card links to Settings");
        cx.simulate_click(settings.center(), Default::default());
        pump(cx);
        assert!(matches!(
            events.borrow().as_slice(),
            [TranscriptEvent::OpenProviderSettings { provider_id }] if provider_id == "xiaomi-cn"
        ));

        // The stamp retires the options.
        state.update(cx, |s, cx| {
            s.transcript[0] = entry(
                "a1",
                vec![
                    choice_part(ChoiceCardState::Chosen, Some("xiaomi-cn")),
                    written_part(),
                ],
            );
            cx.notify();
        });
        pump(cx);
        assert!(cx.debug_bounds("provider-choice-a1#c1-xiaomi").is_none());
        transcript.update(cx, |this, _| {
            assert!(!this.provider_cards.contains_key("a1#c1"));
        });
    }
}
