//! The Agent settings page (automatic-chat-titles ticket 02): the dedicated
//! Agent/Session section for the engine-owned title-task settings. The user
//! picks an optional provider-qualified model (empty = automatic titles
//! disabled) and edits the fixed instruction, both read and saved only
//! through typed RPC — the engine owns `title-settings.json`, validation,
//! and the missing-credentials warning.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};
use holt_proto::{Model, TitleSettingsState};
use holt_rpc::methods;

use crate::{
    composer::ComposerInput,
    popover::{self, Loadable},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

/// One rendered model-choice row. `id: None` is the Disabled row — clearing
/// the model turns automatic titles off without touching anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelRow {
    id: Option<String>,
    title: String,
    detail: String,
    selected: bool,
    /// The stored model no longer resolves against the catalog — shown so
    /// the current value stays visible instead of silently looking disabled
    /// (re-saving it is rejected by the engine until the model resolves).
    unresolved: bool,
}

/// Assemble the model-choice rows: Disabled first, then every resolvable
/// model in catalog order, with the current selection marked. A stored
/// selection missing from the catalog is appended as an unresolved row.
fn model_rows(models: &[Model], selected: Option<&str>) -> Vec<ModelRow> {
    let mut rows = vec![ModelRow {
        id: None,
        title: "Disabled".into(),
        detail: "Keep the first-line title — no background naming request".into(),
        selected: selected.is_none(),
        unresolved: false,
    }];
    let mut matched = selected.is_none();
    for model in models {
        let is_selected = selected == Some(model.id.as_str());
        matched |= is_selected;
        rows.push(ModelRow {
            id: Some(model.id.clone()),
            title: model.label.clone(),
            detail: model.id.clone(),
            selected: is_selected,
            unresolved: false,
        });
    }
    if let Some(id) = selected.filter(|_| !matched) {
        rows.push(ModelRow {
            id: Some(id.to_string()),
            title: id.to_string(),
            detail: "Not in the current provider catalog".into(),
            selected: true,
            unresolved: true,
        });
    }
    rows
}

pub struct AgentPage {
    state: Entity<AppState>,
    settings: Loadable<TitleSettingsState>,
    models: Loadable<Vec<Model>>,
    selected_model: Option<String>,
    instruction: Entity<ComposerInput>,
    save_error: Option<String>,
    task: Option<Task<()>>,
}

impl AgentPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            settings: Loadable::Idle,
            models: Loadable::Idle,
            selected_model: None,
            instruction: cx
                .new(|cx| ComposerInput::new("Instruction sent with the first prompt", cx)),
            save_error: None,
            task: None,
        };
        page.load(cx);
        page
    }

    /// Read the settings record and the resolvable model catalog from the
    /// engine. Recreated per visit (the shell drops the cached page), so a
    /// newly saved provider key or model shows up on the next open.
    /// Echo a state reply (read or save) into the editable fields — the
    /// engine normalizes on save (trim, empty-model disable), so the stored
    /// truth is what the page shows.
    fn apply_state(&mut self, state: TitleSettingsState, cx: &mut Context<Self>) {
        self.selected_model = state.settings.model_id.clone();
        let instruction = state.settings.instruction.clone();
        self.instruction
            .update(cx, |input, cx| input.set_text(instruction, cx));
        self.settings = Loadable::Ready(state);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.settings = Loadable::Error("Engine not connected".into());
            self.models = Loadable::Error("Engine not connected".into());
            return;
        };
        self.settings = Loadable::Loading;
        self.models = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let settings_result = engine
                .client()
                .call(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
                .await;
            let models_result = load_model_catalog(&engine).await;
            this.update(cx, |page, cx| {
                match settings_result {
                    Ok(value) => match serde_json::from_value::<TitleSettingsState>(value) {
                        Ok(state) => page.apply_state(state, cx),
                        Err(error) => page.settings = Loadable::Error(error.to_string()),
                    },
                    // UnknownMethod is version skew, same as the skills page:
                    // name it rather than echoing the raw error.
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        page.settings = Loadable::Error(
                            "Title settings aren't available — the engine doesn't support them yet"
                                .into(),
                        );
                    }
                    Err(error) => page.settings = Loadable::Error(error.to_string()),
                }
                page.models = models_result;
                cx.notify();
            })
            .ok();
        }));
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.save_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let params = serde_json::json!({
            "modelId": self.selected_model,
            "instruction": self.instruction.read(cx).text(),
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SAVE_TITLE_SETTINGS, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<TitleSettingsState>(value) {
                        Ok(state) => {
                            page.apply_state(state, cx);
                            page.save_error = None;
                        }
                        Err(error) => page.save_error = Some(error.to_string()),
                    },
                    Err(error) => page.save_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

/// Every resolvable model across the configured catalog, one LIST_MODELS
/// per concrete provider variant (the same discovery the model picker uses).
async fn load_model_catalog(engine: &crate::state::EngineHandle) -> Loadable<Vec<Model>> {
    let providers = match engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
    {
        Ok(value) => match serde_json::from_value::<Vec<holt_proto::Provider>>(value) {
            Ok(providers) => providers,
            Err(error) => return Loadable::Error(error.to_string()),
        },
        Err(error) => return Loadable::Error(error.to_string()),
    };
    let mut models = Vec::new();
    for provider in providers
        .iter()
        .flat_map(holt_proto::Provider::concrete_providers)
    {
        let Ok(value) = engine
            .client()
            .call(
                methods::LIST_MODELS,
                serde_json::json!({ "providerId": provider.id.0 }),
            )
            .await
        else {
            continue;
        };
        if let Ok(mut provider_models) = serde_json::from_value::<Vec<Model>>(value) {
            models.append(&mut provider_models);
        }
    }
    Loadable::Ready(models)
}

impl Render for AgentPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match (&self.settings, &self.models) {
            (Loadable::Idle, _) | (Loadable::Loading, _) => {
                popover::skeleton_rows("agent-skeleton", &theme, 4, cx.entity_id(), cx)
                    .into_any_element()
            }
            (Loadable::Error(error), _) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            (Loadable::Ready(state), models) => {
                let catalog: &[Model] = models.ready().map(Vec::as_slice).unwrap_or(&[]);
                let rows = model_rows(catalog, self.selected_model.as_deref());

                let mut model_card = widgets::section_card(&theme);
                for (index, row) in rows.iter().enumerate() {
                    let row_id = row.id.clone();
                    let status: Option<AnyElement> = if row.unresolved {
                        Some(widgets::badge(&theme, "unresolved").into_any_element())
                    } else if row.selected {
                        Some(widgets::badge_active(&theme, "In use").into_any_element())
                    } else {
                        None
                    };
                    model_card = model_card.child(
                        widgets::card_row(&theme, index == 0)
                            .id(SharedString::from(format!(
                                "title-model-{}",
                                row.id.as_deref().unwrap_or("disabled")
                            )))
                            .cursor_pointer()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.selected_model = row_id.clone();
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(widgets::row_title(&theme, row.title.clone()))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(
                                                widgets::ROW_DESCRIPTION_SIZE,
                                            ))
                                            .text_color(theme.text_muted)
                                            .child(SharedString::from(row.detail.clone())),
                                    ),
                            )
                            .when_some(status, |card, status| card.child(status)),
                    );
                }

                let mut column = div().flex().flex_col().child(model_card);
                if let Some(warning) = state.warning.clone() {
                    column = column.child(widgets::warning_strip(&theme, warning));
                }

                let restore_theme = theme.clone();
                column = column.child(
                    div()
                        .mt(px(24.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(widgets::field_label(&theme, "Title instruction"))
                        .child(
                            widgets::ghost_action(&theme)
                                .id("restore-title-instruction")
                                .hover(move |style| widgets::ghost_hover(&restore_theme, style))
                                .on_click(cx.listener(|page, _, _, cx| {
                                    page.instruction.update(cx, |input, cx| {
                                        input.set_text(holt_proto::DEFAULT_TITLE_INSTRUCTION, cx);
                                    });
                                    cx.notify();
                                }))
                                .child("Restore default"),
                        ),
                );
                column = column.child(
                    div()
                        .mt(px(8.0))
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.input_glass_bg())
                        .child(self.instruction.clone()),
                );
                column = column.child(widgets::row_description(
                    &theme,
                    "Sent with only the first prompt of a new chat. The reply replaces the \
                     first-line title when it is short and non-empty; failures keep the \
                     fallback silently.",
                ));

                let save_theme = theme.clone();
                column = column.child(
                    div().mt(px(16.0)).flex().flex_row().justify_end().child(
                        widgets::ghost_action(&theme)
                            .id("save-title-settings")
                            .border_1()
                            .border_color(theme.border)
                            .hover(move |style| widgets::ghost_hover(&save_theme, style))
                            .on_click(cx.listener(|page, _, _, cx| page.save(cx)))
                            .child("Save"),
                    ),
                );
                if let Some(error) = self.save_error.clone() {
                    column = column.child(widgets::error_strip(&theme, error));
                }
                column.into_any_element()
            }
        };
        div()
            .id("agent-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Agent", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "How Holt behaves while it works for you — starting with how new \
                         chats are named.",
                    ))
                    .child(
                        div()
                            .mt(px(24.0))
                            .child(widgets::field_label(&theme, "Automatic chat titles")),
                    )
                    .child(div().mt(px(4.0)).child(widgets::row_description(
                        &theme,
                        "A new chat keeps its first-line title immediately, then one \
                             background request to this model can replace it. Your manual \
                             renames always win.",
                    )))
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::ProviderId;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            provider: ProviderId(id.split('/').next().unwrap().into()),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            default_reasoning: None,
            options: Vec::new(),
            custom: false,
        }
    }

    #[test]
    fn disabled_row_comes_first_and_takes_the_empty_selection() {
        let rows = model_rows(&[model("openai/gpt-5.4", "GPT-5.4")], None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, None);
        assert!(rows[0].selected);
        assert!(!rows[1].selected);
    }

    #[test]
    fn the_stored_model_is_marked_in_catalog_order() {
        let models = vec![
            model("anthropic/claude", "Claude"),
            model("openai/gpt-5.4", "GPT-5.4"),
        ];
        let rows = model_rows(&models, Some("openai/gpt-5.4"));
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].selected);
        assert!(!rows[1].selected);
        assert!(rows[2].selected);
        assert!(!rows.iter().any(|row| row.unresolved));
    }

    #[test]
    fn a_selection_missing_from_the_catalog_stays_visible_as_unresolved() {
        let rows = model_rows(&[model("openai/gpt-5.4", "GPT-5.4")], Some("old/provider"));
        let unresolved = rows.iter().find(|row| row.unresolved).unwrap();
        assert_eq!(unresolved.id.as_deref(), Some("old/provider"));
        assert!(unresolved.selected);
        assert!(rows.iter().filter(|row| row.selected).count() == 1);
    }

    #[test]
    fn an_empty_catalog_still_offers_disabled() {
        let rows = model_rows(&[], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, None);
        assert!(rows[0].selected);
    }
}
