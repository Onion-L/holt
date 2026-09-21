//! The Add Provider dialog: tabs, the manual definition form, the top
//! action row, and client-side shape checks.

use super::*;

/// The custom-provider creation form's fields: (key, placeholder).
pub(super) const NEW_PROVIDER_FIELDS: [(&str, &str); 5] = [
    ("id", "Provider id (e.g. acme)"),
    ("name", "Display name"),
    ("baseUrl", "https://acme.example/v1"),
    ("defaultApi", "openai-completions"),
    ("apiKey", "API key (optional)"),
];

/// The Add Provider dialog's tabs (design-v2): the manual form, or the AI
/// setup chat that arrives with V2c.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddProviderTab {
    Manual,
    Ai,
}

/// The page's top action row: the Add Provider primary next to the global
/// reset ghost button, both pushed right by a spacer; the reset opens the
/// confirm dialog.
pub(super) fn top_action_row(theme: &Theme, cx: &mut Context<ProvidersPage>) -> AnyElement {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let hover_theme = theme.clone();
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pb(px(6.0))
        .child(div().flex_1())
        .child(
            action_button(theme)
                .id("open-add-provider")
                .debug_selector(|| "open-add-provider".into())
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(
                    cx.listener(|page, _, _, cx| page.open_add_dialog(AddProviderTab::Manual, cx)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::PLUS)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Add Provider"),
                ),
        )
        .child(
            widgets::ghost_action(theme)
                .id("reset-all-providers")
                .debug_selector(|| "reset-all-providers".into())
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(cx.listener(|page, _, _, cx| {
                    page.confirm_reset_all = true;
                    cx.notify();
                }))
                .child("Reset all providers"),
        )
        .into_any_element()
}

/// The Add Provider dialog (design-v2): tabs over the manual form and the
/// AI setup chat. Rendered through `popover::modal` from the page.
pub(super) fn add_provider_dialog(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let tab = page.add_dialog.unwrap_or(AddProviderTab::Manual);
    // The AI tab is a chat surface: near-window size so the transcript has
    // room; the Manual tab keeps the compact form card.
    let mut card = popover::dialog_card(theme)
        .w(if tab == AddProviderTab::Ai {
            px(760.0)
        } else {
            px(560.0)
        })
        .when(tab == AddProviderTab::Ai, |card| card.h(px(640.0)))
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .child(popover::dialog_title(theme, "Add Provider"))
                .child(div().flex_1())
                .child(
                    widgets::ghost_action(theme)
                        .id("add-provider-close")
                        .debug_selector(|| "add-provider-close".into())
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(|page, _, _, cx| page.close_add_dialog(cx)))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        )
        .child(tab_row(tab, theme, cx));
    card = match tab {
        AddProviderTab::Manual => card.child(manual_tab(
            page.new_provider_error.clone(),
            &page.new_provider_inputs,
            theme,
            cx,
        )),
        AddProviderTab::Ai => card.child(ai_tab(page, theme, cx)),
    };
    card.into_any_element()
}

/// The dialog's tab pills.
pub(super) fn tab_row(
    tab: AddProviderTab,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let mut row = div().flex().items_center().gap(px(6.0));
    for (candidate, label) in [
        (AddProviderTab::Manual, "Manual"),
        (AddProviderTab::Ai, "AI"),
    ] {
        let selected = candidate == tab;
        let hover_theme = theme.clone();
        let mut pill = div()
            .id(match candidate {
                AddProviderTab::Manual => "add-provider-tab-manual",
                AddProviderTab::Ai => "add-provider-tab-ai",
            })
            .debug_selector(move || match candidate {
                AddProviderTab::Manual => "add-provider-tab-manual".into(),
                AddProviderTab::Ai => "add-provider-tab-ai".into(),
            })
            .cursor_pointer()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .border_1()
            .text_size(crate::typography::ui_rems(11.5))
            .child(label);
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
        row = row.child(pill.on_click(cx.listener(move |page, _, _, cx| {
            if page.add_dialog != Some(candidate) {
                page.add_dialog = Some(candidate);
                if candidate == AddProviderTab::Ai {
                    page.prepare_setup(cx);
                }
                cx.notify();
            }
        })));
    }
    row.into_any_element()
}

/// The manual tab: the custom-provider definition form (moved from the old
/// page-bottom section).
pub(super) fn manual_tab(
    error: Option<String>,
    inputs: &HashMap<&'static str, Entity<ComposerInput>>,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let field = |key: &'static str, label: &str| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::field_label(theme, label))
            .children(inputs.get(key).map(|input| {
                bordered_input(theme, input.clone())
                    .w_full()
                    .into_any_element()
            }))
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(field("id", "Provider id"))
                .child(field("name", "Display name")),
        )
        .child(field("baseUrl", "Base URL"))
        .child(field("defaultApi", "Default API dialect"))
        .child(field("apiKey", "API key (optional)"))
        .children(error.map(|message| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.danger_muted.opacity(0.9))
                .child(SharedString::from(message))
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    action_button(theme)
                        .id("save-custom-provider")
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(move |page, _, _, cx| page.save_custom_provider(cx)))
                        .child("Save provider"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-custom-provider")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page, _, _, cx| page.close_add_dialog(cx)))
                        .child("Cancel"),
                ),
        )
        .into_any_element()
}

/// Client-side checks for a new custom provider — the quick feedback before
/// the engine's authoritative validation replies.
/// Plaintext http would carry the key and the conversation in the clear,
/// so the form mirrors the engine's rule: http is for local servers only.
pub(super) fn base_url_problem(base_url: &str) -> Option<String> {
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Some("Base URL must start with http:// or https://".into());
    }
    if let Some(rest) = base_url.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or_default();
        let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
        let host = host.trim_matches(|character| character == '[' || character == ']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !loopback {
            return Some("Plaintext http is allowed only for localhost endpoints".into());
        }
    }
    None
}

pub(super) fn new_provider_problem(id: &str, base_url: &str, default_api: &str) -> Option<String> {
    if id.is_empty() {
        return Some("Provider id is required".into());
    }
    if id.contains('/') {
        return Some("Provider id cannot contain '/'".into());
    }
    if let Some(problem) = base_url_problem(base_url) {
        return Some(problem);
    }
    if default_api.is_empty() {
        return Some("Default API dialect is required".into());
    }
    None
}

impl ProvidersPage {
    pub(super) fn open_add_dialog(&mut self, tab: AddProviderTab, cx: &mut Context<Self>) {
        if tab == AddProviderTab::Ai {
            self.prepare_setup(cx);
        }
        self.add_dialog = Some(tab);
        for (key, placeholder) in NEW_PROVIDER_FIELDS {
            // The key field is masked like every other key entry — the
            // manual form collects a credential, not chat text.
            let secret = key == "apiKey";
            self.new_provider_inputs.entry(key).or_insert_with(|| {
                cx.new(|cx| {
                    if secret {
                        ComposerInput::new_secret(placeholder, cx)
                    } else {
                        ComposerInput::new(placeholder, cx)
                    }
                })
            });
        }
        cx.notify();
    }

    pub(super) fn close_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.add_dialog = None;
        self.new_provider_error = None;
        // Dropping the view + the doc watch stops all background work; the
        // next open re-prepares from a fresh subscription. The session's
        // applied/error memories go with it — a reopened dialog lists only
        // what its own session wrote.
        self.setup_transcript_view = None;
        self.setup_doc_empty = true;
        self.setup_proposal_signature = (0, 0, 0);
        self.setup_queue_task = None;
        self.setup_queue = None;
        self.setup_applied.clear();
        self.setup_apply_errors.clear();
        self.setup_applying = None;
        self.setup_key_request = None;
        self.setup_key_input = None;
        self.setup_key_error = None;
        self.setup_key_settling = false;
        // The setup chat is session-scoped: closing the dialog ends it.
        // The delete cancels any in-flight turn and drops the transcript
        // and its stored proposals — nothing carries into the next open.
        if let Some(chat_id) = self.setup_chat.take() {
            self.state
                .update(cx, |state, _| state.unwatch_subagent_doc(&chat_id));
            if let Some(engine) = self.state.read(cx).engine().cloned() {
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
        }
        cx.notify();
    }

    // -- The AI tab (V2c) --------------------------------------------------

    pub(super) fn save_custom_provider(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let field = |page: &Self, key: &str, cx: &Context<Self>| {
            page.new_provider_inputs
                .get(key)
                .map(|input| input.read(cx).text().trim().to_string())
                .unwrap_or_default()
        };
        let id = field(self, "id", cx);
        let name = {
            let name = field(self, "name", cx);
            if name.is_empty() { id.clone() } else { name }
        };
        let base_url = field(self, "baseUrl", cx);
        let default_api = field(self, "defaultApi", cx);
        // The optional key: non-empty rides the same credential path as
        // every other key entry right after the definition; empty means
        // "leave any stored key untouched" — never a clearing write.
        let api_key = field(self, "apiKey", cx);
        if let Some(problem) = new_provider_problem(&id, &base_url, &default_api) {
            self.new_provider_error = Some(problem);
            cx.notify();
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_CUSTOM_PROVIDER,
                    serde_json::json!({
                        "id": id,
                        "name": name,
                        "baseUrl": base_url,
                        "defaultApi": default_api,
                    }),
                )
                .await;
            let key_result = match (&result, api_key.is_empty()) {
                // The definition landed and a key was typed: write it
                // through the credential path before closing.
                (Ok(_), false) => Some(
                    engine
                        .client()
                        .call(
                            methods::SAVE_PROVIDER_KEY,
                            serde_json::json!({ "providerId": id, "key": api_key }),
                        )
                        .await,
                ),
                _ => None,
            };
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_add_dialog(cx);
                        for input in page.new_provider_inputs.values() {
                            input.update(cx, |input, cx| input.set_text("", cx));
                        }
                        crate::pickers::bump_provider_catalog(cx);
                        page.load(cx);
                        // The definition is saved and the dialog closed; a
                        // failed key write is a storage fault, not a form
                        // error — surface it at the window top for a retry.
                        if let Some(Err(error)) = key_result {
                            page.fail(error.to_string(), cx);
                        }
                    }
                    Err(error) => page.new_provider_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn remove_custom_provider(
        &mut self,
        provider: String,
        org_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_CUSTOM_PROVIDER,
                    serde_json::json!({ "providerId": provider }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.close_panel_forms();
                        page.expanded = None;
                        page.begin_collapse(org_id, cx);
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
    use crate::settings::providers::test_support::*;

    #[test]
    fn new_provider_forms_check_shape_before_the_rpc() {
        assert!(new_provider_problem("", "https://x.example", "openai-completions").is_some());
        assert!(new_provider_problem("a/b", "https://x.example", "openai-completions").is_some());
        assert!(new_provider_problem("acme", "x.example", "openai-completions").is_some());
        assert!(new_provider_problem("acme", "https://x.example", "").is_some());
        assert!(
            new_provider_problem("acme", "https://x.example/v1", "openai-completions").is_none()
        );
    }

    /// The fresh-install dead end: no provider configured means the setup
    /// assistant has no model to run on. The tab must offer the way out —
    /// the manual form — not just the dead-end error strip.
    #[gpui::test]
    fn the_manual_tab_escape_hatch_when_no_provider_is_configured(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .unconfigured
            .store(true, std::sync::atomic::Ordering::SeqCst);
        harness.click("open-add-provider");
        harness.click("add-provider-tab-ai");
        harness.pump();

        assert!(
            harness
                .visual
                .debug_bounds("setup-bootstrap-manual")
                .is_some(),
            "the dead end offers the manual form"
        );
        harness.click("setup-bootstrap-manual");
        let tab = harness
            .page
            .update(&mut *harness.visual, |page, _| page.add_dialog);
        assert_eq!(
            tab,
            Some(AddProviderTab::Manual),
            "the escape hatch switches to the manual tab"
        );
    }

    /// Fills the manual form and saves. Returns the fake's recorded calls.
    fn fill_and_save_manual(harness: &mut SetupHarness<'_>, key: &str) -> Vec<serde_json::Value> {
        harness.click("open-add-provider");
        harness.click("add-provider-tab-manual");
        harness.pump();
        harness.page.update(&mut *harness.visual, |page, cx| {
            for (field, text) in [
                ("id", "acme"),
                ("name", "Acme Labs"),
                ("baseUrl", "https://acme.example/v1"),
                ("defaultApi", "openai-completions"),
                ("apiKey", key),
            ] {
                let input = page
                    .new_provider_inputs
                    .get(field)
                    .unwrap_or_else(|| panic!("the {field} field exists"))
                    .clone();
                input.update(cx, |input, cx| input.set_text(text, cx));
            }
            page.save_custom_provider(cx);
        });
        harness.pump();
        harness.engine.custom_saves.lock().unwrap().clone()
    }

    /// The manual form's optional key field (issue 03): a non-empty value
    /// rides the same credential path as every other key entry — the
    /// provider save and the key save land together, no model involved.
    #[gpui::test]
    fn the_manual_tab_key_field_writes_the_credential_path(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        let saves = fill_and_save_manual(&mut harness, "sk-manual-secret");
        assert_eq!(saves.len(), 1, "the provider definition saved");
        let keys = harness.engine.saved_keys.lock().unwrap().clone();
        assert_eq!(
            keys.len(),
            1,
            "the typed key went through SaveProviderKey: {keys:?}"
        );
        assert_eq!(keys[0]["providerId"], "acme");
        assert_eq!(keys[0]["key"], "sk-manual-secret");
        assert_eq!(
            harness
                .engine
                .keys
                .lock()
                .unwrap()
                .get("acme")
                .map(String::as_str),
            Some("sk-manual-secret"),
            "the store holds the key the reveal path reads"
        );
        let open = harness
            .page
            .update(&mut *harness.visual, |page, _| page.add_dialog.is_some());
        assert!(!open, "the dialog closed on success");

        // The panel behind the dialog shows the stored state: expanding
        // acme's panel fills its masked key input through the reveal path.
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.pump();
        let stored = harness.page.update(&mut *harness.visual, |page, cx| {
            page.inputs
                .get("acme")
                .map(|input| input.read(cx).text().to_string())
        });
        assert_eq!(
            stored.as_deref(),
            Some("sk-manual-secret"),
            "the panel's masked input carries the stored key"
        );
    }

    /// An empty key field never writes — re-saving a definition over an
    /// existing provider leaves its stored key exactly as it was.
    #[gpui::test]
    fn an_empty_manual_key_field_leaves_the_stored_key_untouched(cx: &mut gpui::TestAppContext) {
        let mut harness = setup_dialog_harness(cx);
        harness
            .engine
            .keys
            .lock()
            .unwrap()
            .insert("acme".to_string(), "sk-original".to_string());
        let saves = fill_and_save_manual(&mut harness, "");
        assert_eq!(saves.len(), 1, "the definition still saves");
        assert!(
            harness.engine.saved_keys.lock().unwrap().is_empty(),
            "an empty field makes no key call"
        );
        assert_eq!(
            harness
                .engine
                .keys
                .lock()
                .unwrap()
                .get("acme")
                .map(String::as_str),
            Some("sk-original"),
            "the stored key survived the re-save"
        );
    }
}
