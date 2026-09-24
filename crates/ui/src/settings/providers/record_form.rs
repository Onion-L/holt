//! The manual model-record form (ADR-0029's manual half): field
//! construction, validation, the API-dialect dropdown, and the dialog.

use super::*;

/// The model-record form's single-line fields, in render order:
/// (key, label, placeholder). Laid out two per row. No `baseUrl` field —
/// a record added under a provider rides that provider's endpoint; the
/// engine fills the default, and Advanced JSON is the override hatch.
/// `api` is not a text field: the dialect set is closed (the engine's
/// registered list), so it rides a dropdown.
pub(super) const RECORD_FIELDS: [(&str, &str, &str); 8] = [
    ("id", "Model ID", "acme-1"),
    ("name", "Name", "Acme 1"),
    ("contextWindow", "Context window", "200000"),
    ("maxTokens", "Max tokens", "8192"),
    ("inputCost", "Input cost /M", "0.0"),
    ("outputCost", "Output cost /M", "0.0"),
    ("cacheReadCost", "Cache read /M", "0.0"),
    ("cacheWriteCost", "Cache write /M", "0.0"),
];

/// The mounted model-record form. One panel is expanded at a time, so one
/// instance serves the whole page; it targets that panel's active variant.
pub(super) struct RecordForm {
    provider: String,
    inputs: HashMap<String, Entity<ComposerInput>>,
    /// The picked API dialect (the dropdown's selection; the set comes
    /// from the engine's `ListApiDialects`).
    api: String,
    reasoning: bool,
    image: bool,
    error: Option<String>,
}

/// The record form's API-dialect dropdown: a closed set (the engine's
/// registered dialects), so a picker instead of a typo-prone text field.
/// The trigger mirrors [`bordered_input`]'s shape to sit flush in the
/// form's grid; the menu opens downward at [`popover::ABOVE_MODAL_PRIORITY`]
/// because the dialog itself is a modal.
pub(super) fn record_api_dropdown(
    page: &ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let selected = page
        .record_form
        .as_ref()
        .map(|form| form.api.clone())
        .unwrap_or_default();
    let rows: Vec<AnyElement> = page
        .api_dialects
        .ready()
        .map(|dialects| {
            dialects
                .iter()
                .map(|dialect| {
                    let picked = dialect.clone();
                    let selector = format!("record-api-option-{dialect}");
                    popover::menu_row(
                        theme,
                        *dialect == selected,
                        format!("record-api-row-{dialect}"),
                    )
                    .id(SharedString::from(format!("record-api-{dialect}")))
                    .debug_selector(move || selector.clone())
                    .on_click(
                        cx.listener(move |page, _, _, cx| page.pick_record_api(picked.clone(), cx)),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(12.0))
                            .child(SharedString::from(dialect.clone())),
                    )
                    .into_any_element()
                })
                .collect()
        })
        .unwrap_or_default();
    let menu = popover::popover_card(theme)
        .w(px(260.0))
        .occlude()
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_record_api_menu(cx)))
        .children(rows)
        .into_any_element();
    div()
        .id("record-api-dropdown")
        .debug_selector(|| "record-api-dropdown".into())
        .relative()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input_glass_bg())
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.record_api_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| page.toggle_record_api_menu(cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text)
                .child(SharedString::from(selected)),
        )
        .child(
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .when_some(page.record_api_menu.get(), |trigger, _| {
            trigger.child(popover::anchored_menu_below_end_with_priority(
                "record-api-menu",
                menu,
                page.record_api_menu.closing_since(),
                popover::ABOVE_MODAL_PRIORITY,
            ))
        })
        .into_any_element()
}

/// The Add-model-record dialog (`popover::modal` from the page), opened
/// from the Models header's "+ Add model" action. Basic fields up front;
/// `thinkingLevelMap`/`compat`/`headers` ride the advanced JSON textarea —
/// a full structured editor is not worth the surface.
pub(super) fn record_form_dialog(
    page: &mut ProvidersPage,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let Some(form) = page.record_form.as_ref() else {
        return div().into_any_element();
    };
    let field_input = |key: &str| form.inputs.get(key).cloned();
    // Built ahead of the closures below: they hold `cx` for their
    // listeners, so the dropdown (which registers its own) must borrow
    // `cx` first.
    let api_dropdown = record_api_dropdown(page, theme, cx);
    let pair = |left: (&str, &str), right: Option<(&str, &str)>| {
        div()
            .flex()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::field_label(theme, left.1))
                    .children(
                        field_input(left.0)
                            .map(|input| bordered_input(theme, input).w_full().into_any_element()),
                    ),
            )
            .children(right.map(|(key, label)| {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(widgets::field_label(theme, label))
                    .children(
                        field_input(key)
                            .map(|input| bordered_input(theme, input).w_full().into_any_element()),
                    )
                    .into_any_element()
            }))
    };
    let flag_row = |key: &'static str, label: &str, on: bool| {
        div()
            .id(SharedString::from(format!("record-flag-{key}")))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(8.0))
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_record_flag(key, cx)))
            .child(widgets::checkbox(
                theme,
                if on {
                    widgets::CheckboxState::Checked
                } else {
                    widgets::CheckboxState::Unchecked
                },
            ))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(label.to_string()),
            )
    };
    let mut card = popover::dialog_card(theme)
        // A mask click dismisses — unless the API dropdown is open, whose
        // rows float outside this card's bounds and must survive the same
        // press that closes the menu.
        .on_mouse_down_out(cx.listener(|page, _, _, cx| {
            if page.record_api_menu.get().is_none() {
                page.record_form = None;
                cx.notify();
            }
        }))
        .w(px(560.0))
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_center()
                .child(popover::dialog_title(theme, "Add model record"))
                .child(div().flex_1())
                .child(
                    widgets::ghost_action(theme)
                        .id("record-form-close")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                ),
        );
    card = card
        .child(pair(("id", "Model ID"), Some(("name", "Name"))))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(theme, "API dialect"))
                .child(api_dropdown),
        )
        .child(pair(
            ("contextWindow", "Context window"),
            Some(("maxTokens", "Max tokens")),
        ))
        .child(pair(
            ("inputCost", "Input cost /M"),
            Some(("outputCost", "Output cost /M")),
        ))
        .child(pair(
            ("cacheReadCost", "Cache read /M"),
            Some(("cacheWriteCost", "Cache write /M")),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(20.0))
                .child(flag_row("reasoning", "Reasoning", form.reasoning))
                .child(flag_row("image", "Image input", form.image)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::field_label(theme, "Advanced JSON"))
                .children(field_input("advanced").map(|input| {
                    div()
                        .id("record-advanced")
                        .h(px(80.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .flex()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.input_glass_bg())
                        .overflow_y_scroll()
                        .occlude()
                        .child(input)
                        .into_any_element()
                })),
        )
        .children(form.error.clone().map(|message| {
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
                        .id("save-record")
                        .debug_selector(|| "save-record".into())
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(|page: &mut ProvidersPage, _, _, cx| {
                            page.save_record(cx);
                        }))
                        .child("Save record"),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("cancel-record")
                        .hover(move |style| widgets::ghost_hover(theme, style))
                        .on_click(cx.listener(move |page: &mut ProvidersPage, _, _, cx| {
                            page.record_form = None;
                            cx.notify();
                        }))
                        .child("Cancel"),
                ),
        );
    card.into_any_element()
}

/// Builds the model record the engine expects from the form's texts:
/// numbers parse, costs default to zero, and the advanced JSON (when
/// present) must be an object whose keys ride along — except the form's
/// own fields, which the advanced object can never override. `baseUrl`
/// is not a form field: empty means omitted, and the engine fills the
/// provider's default endpoint (Advanced JSON may still set it).
pub(super) fn build_record_json(
    provider: &str,
    texts: &HashMap<String, String>,
    reasoning: bool,
    image: bool,
) -> Result<serde_json::Value, String> {
    let text = |key: &str| texts.get(key).cloned().unwrap_or_default();
    let id = text("id");
    if id.is_empty() {
        return Err("Model ID is required".into());
    }
    let name = {
        let name = text("name");
        if name.is_empty() { id.clone() } else { name }
    };
    let base_url = text("baseUrl");
    if !base_url.is_empty() && !base_url.starts_with("http://") && !base_url.starts_with("https://")
    {
        return Err("Base URL must start with http:// or https://".into());
    }
    let api = text("api");
    if api.is_empty() {
        return Err("API dialect is required".into());
    }
    let number = |key: &str| -> Result<u64, String> {
        let raw = text(key);
        raw.parse::<u64>()
            .map_err(|_| format!("{key} must be a whole number"))
    };
    let context_window = number("contextWindow")?;
    if context_window == 0 {
        return Err("Context window must be greater than zero".into());
    }
    let max_tokens = number("maxTokens")?;
    let cost = |key: &str| -> Result<f64, String> {
        let raw = text(key);
        if raw.is_empty() {
            return Ok(0.0);
        }
        raw.parse::<f64>()
            .map_err(|_| format!("{key} must be a number"))
    };
    let mut input = vec!["text".to_string()];
    if image {
        input.push("image".to_string());
    }
    let mut record = serde_json::json!({
        "id": id,
        "name": name,
        "api": api,
        "provider": provider,
        "reasoning": reasoning,
        "input": input,
        "cost": {
            "input": cost("inputCost")?,
            "output": cost("outputCost")?,
            "cacheRead": cost("cacheReadCost")?,
            "cacheWrite": cost("cacheWriteCost")?,
        },
        "contextWindow": context_window,
        "maxTokens": max_tokens,
    });
    // No baseUrl input in the form: omitted means the engine fills the
    // provider's default endpoint; the advanced JSON below is the override
    // hatch (per-model gateways keep working).
    if !base_url.is_empty() {
        record["baseUrl"] = serde_json::Value::String(base_url);
    }
    let advanced = text("advanced");
    if !advanced.is_empty() {
        let parsed: serde_json::Value = serde_json::from_str(&advanced)
            .map_err(|error| format!("Advanced JSON does not parse: {error}"))?;
        let Some(object) = parsed.as_object() else {
            return Err("Advanced JSON must be an object".into());
        };
        if let Some(target) = record.as_object_mut() {
            for (key, value) in object {
                // The form's fields are authoritative — the advanced object
                // carries only what the form does not own (baseUrl included:
                // omitting it is the default-endpoint case).
                if ![
                    "id",
                    "name",
                    "api",
                    "provider",
                    "reasoning",
                    "input",
                    "cost",
                    "contextWindow",
                    "maxTokens",
                ]
                .contains(&key.as_str())
                {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok(record)
}

impl ProvidersPage {
    pub(super) fn open_record_form(&mut self, provider: String, cx: &mut Context<Self>) {
        let mut inputs = HashMap::new();
        for (key, _, placeholder) in RECORD_FIELDS {
            inputs.insert(
                key.to_string(),
                cx.new(|cx| ComposerInput::new(placeholder, cx)),
            );
        }
        inputs.insert(
            "advanced".to_string(),
            cx.new(|cx| {
                ComposerInput::new(
                    "{ \"thinkingLevelMap\": …, \"compat\": … } — optional JSON",
                    cx,
                )
            }),
        );
        // A stale menu from a previous form must not carry over.
        self.record_api_menu = Popup::default();
        self.load_api_dialects(cx);
        self.record_form = Some(RecordForm {
            provider,
            inputs,
            api: "openai-completions".into(),
            reasoning: false,
            image: false,
            error: None,
        });
        cx.notify();
    }

    /// The dialect dropdown's options: the engine's registered api
    /// dialects (pi-core's registry), loaded once per page. Detached — a
    /// cancelled load would wedge the list in Loading forever.
    pub(super) fn load_api_dialects(&mut self, cx: &mut Context<Self>) {
        if matches!(self.api_dialects, Loadable::Ready(_) | Loadable::Loading) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.api_dialects = Loadable::Loading;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_API_DIALECTS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.api_dialects = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|error| Loadable::Error(error.to_string())),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn toggle_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.take_press_was_open() || self.record_api_menu.is_open() {
            self.close_record_api_menu(cx);
        } else {
            self.record_api_menu.open(());
        }
        cx.notify();
    }

    pub(super) fn close_record_api_menu(&mut self, cx: &mut Context<Self>) {
        if self.record_api_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut ProvidersPage| &mut page.record_api_menu);
            cx.notify();
        }
    }

    pub(super) fn pick_record_api(&mut self, dialect: String, cx: &mut Context<Self>) {
        if let Some(form) = self.record_form.as_mut() {
            form.api = dialect;
        }
        self.close_record_api_menu(cx);
        cx.notify();
    }

    pub(super) fn toggle_record_flag(&mut self, flag: &str, cx: &mut Context<Self>) {
        let Some(form) = self.record_form.as_mut() else {
            return;
        };
        if flag == "reasoning" {
            form.reasoning = !form.reasoning;
        } else if flag == "image" {
            form.image = !form.image;
        }
        cx.notify();
    }

    pub(super) fn save_record(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(form) = self.record_form.as_ref() else {
            return;
        };
        let mut texts = HashMap::new();
        for (key, input) in &form.inputs {
            texts.insert(key.clone(), input.read(cx).text().trim().to_string());
        }
        let provider = form.provider.clone();
        texts.insert("api".to_string(), form.api.clone());
        let record = match build_record_json(&provider, &texts, form.reasoning, form.image) {
            Ok(record) => record,
            Err(problem) => {
                if let Some(form) = self.record_form.as_mut() {
                    form.error = Some(problem);
                }
                cx.notify();
                return;
            }
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_MODEL_RECORD,
                    serde_json::json!({
                        "providerId": provider.clone(),
                        "record": record,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.record_form = None;
                        crate::pickers::bump_provider_catalog(cx);
                        page.load_models(&provider, true, cx);
                        page.load_hidden(&provider, true, cx);
                    }
                    Err(error) => {
                        if let Some(form) = page.record_form.as_mut() {
                            form.error = Some(error.to_string());
                        }
                    }
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

    fn record_texts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn a_complete_form_builds_a_servable_record() {
        let texts = record_texts(&[
            ("id", "acme-1"),
            ("baseUrl", "https://acme.example/v1"),
            ("api", "openai-completions"),
            ("contextWindow", "321000"),
            ("maxTokens", "16384"),
            ("inputCost", "1.5"),
            ("advanced", "{\"thinkingLevelMap\": {\"high\": \"high\"}}"),
        ]);
        let record = build_record_json("acme", &texts, true, true).unwrap();
        assert_eq!(record["id"], "acme-1");
        // A blank name falls back to the id; costs default to zero.
        assert_eq!(record["name"], "acme-1");
        assert_eq!(record["provider"], "acme");
        // An explicit baseUrl (the endpoint-fix case) rides along.
        assert_eq!(record["baseUrl"], "https://acme.example/v1");
        assert_eq!(record["input"], serde_json::json!(["text", "image"]));
        assert_eq!(record["cost"]["output"], 0.0);
        assert_eq!(record["cost"]["input"], 1.5);
        assert_eq!(record["contextWindow"], 321_000);
        assert_eq!(
            record["thinkingLevelMap"]["high"],
            serde_json::json!("high")
        );
    }

    #[test]
    fn a_form_without_a_base_url_leaves_it_to_the_engine() {
        // The form no longer asks for baseUrl: adding a model to a provider
        // rides the provider's endpoint, so the record omits the field and
        // the engine fills the default.
        let texts = record_texts(&[
            ("id", "acme-1"),
            ("api", "openai-completions"),
            ("contextWindow", "1000"),
            ("maxTokens", "100"),
        ]);
        let record = build_record_json("acme", &texts, false, false).unwrap();
        assert!(record.get("baseUrl").is_none());
        // Advanced JSON remains the override hatch for per-model gateways.
        let gateway = record_texts(&[
            ("id", "acme-1"),
            ("api", "openai-completions"),
            ("contextWindow", "1000"),
            ("maxTokens", "100"),
            ("advanced", "{\"baseUrl\": \"https://gateway.example/v1\"}"),
        ]);
        let record = build_record_json("acme", &gateway, false, false).unwrap();
        assert_eq!(record["baseUrl"], "https://gateway.example/v1");
    }

    #[test]
    fn record_forms_reject_bad_input_before_the_rpc() {
        let base = |pairs: &[(&str, &str)]| record_texts(pairs);
        let good = |extra: &[(&str, &str)]| {
            let mut texts = base(&[
                ("id", "acme-1"),
                ("baseUrl", "https://acme.example/v1"),
                ("api", "openai-completions"),
                ("contextWindow", "1000"),
                ("maxTokens", "100"),
            ]);
            for (key, value) in extra {
                texts.insert(key.to_string(), value.to_string());
            }
            texts
        };
        assert!(
            build_record_json("acme", &base(&[]), false, false)
                .unwrap_err()
                .contains("Model ID is required")
        );
        assert!(
            build_record_json("acme", &good(&[("baseUrl", "ftp://nope")]), false, false).is_err()
        );
        // No baseUrl is fine — the engine inherits the provider's endpoint.
        assert!(build_record_json("acme", &good(&[("baseUrl", "")]), false, false).is_ok());
        assert!(build_record_json("acme", &good(&[("contextWindow", "0")]), false, false).is_err());
        assert!(
            build_record_json("acme", &good(&[("contextWindow", "lots")]), false, false)
                .unwrap_err()
                .contains("whole number")
        );
        assert!(
            build_record_json("acme", &good(&[("outputCost", "cheap")]), false, false)
                .unwrap_err()
                .contains("must be a number")
        );
        assert!(
            build_record_json("acme", &good(&[("advanced", "[1,2]")]), false, false)
                .unwrap_err()
                .contains("must be an object")
        );
        // The advanced object cannot smuggle the record's identity fields.
        let smuggled = good(&[("advanced", "{\"provider\": \"evil\", \"id\": \"evil-1\"}")]);
        let record = build_record_json("acme", &smuggled, false, false).unwrap();
        assert_eq!(record["provider"], "acme");
    }

    /// The record form's API dialect is a dropdown over the engine's
    /// registered dialects, not a free-text field: the pick lands in the
    /// saved record.
    #[gpui::test]
    fn the_record_form_picks_the_api_dialect_from_a_dropdown(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        // The panel clips its content behind the expand animation's height
        // (wall-clock driven — a parked test clock never finishes it), so
        // the rows' hitboxes stay clipped away. Reduce motion: animations
        // complete on the first layout.
        harness
            .visual
            .update(|_window, cx| cx.set_reduce_motion(true));
        harness.click("provider-row-0");
        harness.click("toggle-record-form");

        // The dropdown defaults to openai-completions.
        let api = harness.page.update(&mut *harness.visual, |page, _| {
            page.record_form.as_ref().map(|form| form.api.clone())
        });
        assert_eq!(api.as_deref(), Some("openai-completions"));

        harness.click("record-api-dropdown");
        harness.click("record-api-option-anthropic-messages");
        let api = harness.page.update(&mut *harness.visual, |page, _| {
            page.record_form.as_ref().map(|form| form.api.clone())
        });
        assert_eq!(api.as_deref(), Some("anthropic-messages"));

        harness.page.update(&mut *harness.visual, |page, cx| {
            let form = page.record_form.as_ref().expect("the record form");
            for (key, value) in [
                ("id", "acme-9"),
                ("contextWindow", "200000"),
                ("maxTokens", "8192"),
            ] {
                form.inputs[key].update(cx, |input, cx| input.set_text(value, cx));
            }
        });
        harness.click("save-record");
        harness.pump();

        let records = harness.engine.records.lock().unwrap().clone();
        assert_eq!(records.len(), 1, "one SaveModelRecord");
        assert_eq!(records[0]["record"]["api"], "anthropic-messages");
    }
}
