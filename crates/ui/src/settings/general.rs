//! The General settings page (automatic-chat-titles ticket 02): everyday
//! preferences that aren't tied to a provider or the agent runtime, starting
//! with the engine-owned title-task settings. The user picks an optional
//! provider-qualified model (empty = automatic titles disabled) and edits the
//! instruction's style notes, both read and saved only through typed RPC —
//! the engine owns `title-settings.json`, validation, and the
//! missing-credentials warning.
//!
//! It also hosts the Web search group ([`web_search`]), an entity of its
//! own so its dropdown and key field stay out of this page.

use gpui::{
    AnyElement, Context, Entity, IntoElement, MouseButton, Render, SharedString, Task, Window, div,
    prelude::*, px,
};
use holt_proto::{Model, Provider, TitleSettingsState};
use holt_rpc::methods;

mod web_search;

use crate::{
    composer::ComposerInput,
    popover::{self, Loadable, Popup},
    settings::{self, SavePolicy, widgets},
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

fn effective_instruction(custom_enabled: bool, instruction: &str) -> String {
    if custom_enabled && !instruction.trim().is_empty() {
        instruction.to_string()
    } else {
        holt_proto::DEFAULT_TITLE_INSTRUCTION.to_string()
    }
}

fn configured_providers(providers: &[Provider]) -> Vec<Provider> {
    providers
        .iter()
        .flat_map(Provider::concrete_providers)
        .filter(|provider| provider.configured)
        .collect()
}

/// What a stored-key field (Web search) currently shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyField {
    /// The engine's masked key — untouched.
    Stored,
    /// The raw stored key, fetched on demand by the reveal RPC.
    Revealed,
    /// The user's unsaved edit.
    Draft,
}

pub struct GeneralPage {
    state: Entity<AppState>,
    settings: Loadable<TitleSettingsState>,
    models: Loadable<Vec<Model>>,
    selected_model: Option<String>,
    model_menu: Popup<()>,
    custom_instruction_enabled: bool,
    instruction: Entity<ComposerInput>,
    save_error: Option<String>,
    task: Option<Task<()>>,
    /// The Web search group (web-tools ticket 07).
    web_search: Entity<web_search::WebSearchGroup>,
}

impl GeneralPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let web_search = cx.new(|cx| web_search::WebSearchGroup::new(state.clone(), cx));
        let mut page = Self {
            state,
            settings: Loadable::Idle,
            models: Loadable::Idle,
            selected_model: None,
            model_menu: Popup::default(),
            custom_instruction_enabled: false,
            instruction: cx
                .new(|cx| ComposerInput::new("Instruction sent with the first prompt", cx)),
            save_error: None,
            task: None,
            web_search,
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
        self.custom_instruction_enabled =
            instruction.trim() != holt_proto::DEFAULT_TITLE_INSTRUCTION;
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
            let models_result = match load_providers(&engine).await {
                Ok(providers) => load_model_catalog(&engine, &providers).await,
                Err(error) => Loadable::Error(error),
            };
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
        let instruction = effective_instruction(
            self.custom_instruction_enabled,
            self.instruction.read(cx).text(),
        );
        self.save_title_settings(instruction, false, cx);
    }

    /// Commit a model pick on its own: the stored instruction rides along
    /// unchanged, and an unsaved style draft survives the reply.
    fn save_model(&mut self, cx: &mut Context<Self>) {
        let Some(instruction) = self
            .settings
            .ready()
            .map(|state| state.settings.instruction.clone())
        else {
            return;
        };
        self.save_title_settings(instruction, true, cx);
    }

    fn save_title_settings(
        &mut self,
        instruction: String,
        keep_draft: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.save_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let params = serde_json::json!({
            "modelId": self.selected_model,
            "instruction": instruction,
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
                            if keep_draft {
                                page.selected_model = state.settings.model_id.clone();
                                page.settings = Loadable::Ready(state);
                            } else {
                                page.apply_state(state, cx);
                            }
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

    /// engine-backed title settings above: it renders and works even when
    /// that load fails. Toggles persist immediately through the settings
    /// store, which the notification controller reads at event time.
    fn render_notifications(theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let ui_settings = settings::current(cx);
        let master_on = ui_settings.completion_notifications;
        let sound_on = ui_settings.completion_notification_sound;

        let sound_switch = div()
            .id("completion-notification-sound-toggle")
            .debug_selector(|| "completion-notification-sound-toggle".into())
            .flex_none()
            .child(widgets::toggle_switch(theme, sound_on));
        // While the master is off the control is visually non-interactive;
        // the stored sound choice is untouched and honored again on
        // re-enable.
        let sound_switch = if master_on {
            sound_switch
                .cursor_pointer()
                .on_click(cx.listener(|_, _, _, cx| {
                    settings::update(SavePolicy::Immediate, cx, |settings| {
                        settings.completion_notification_sound =
                            !settings.completion_notification_sound;
                    });
                    cx.notify();
                }))
        } else {
            sound_switch.opacity(0.4)
        };

        div()
            .mt(px(GROUP_GAP))
            .child(group_header(
                theme,
                "Notifications",
                None,
                "Device-local. Banners appear only while no Holt window is active.",
            ))
            .child(
                group_rows()
                    .child(
                        group_row()
                            .child(row_text(
                                theme,
                                "Completion notifications",
                                "Show a system banner when a background Turn succeeds or fails.",
                            ))
                            .child(
                                div()
                                    .id("completion-notifications-toggle")
                                    .debug_selector(|| "completion-notifications-toggle".into())
                                    .flex_none()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        settings::update(SavePolicy::Immediate, cx, |settings| {
                                            settings.completion_notifications =
                                                !settings.completion_notifications;
                                        });
                                        cx.notify();
                                    }))
                                    .child(widgets::toggle_switch(theme, master_on)),
                            ),
                    )
                    .child(
                        group_row()
                            .child(row_text(
                                theme,
                                "Play sound",
                                "Play the system notification sound with each banner.",
                            ))
                            .child(sound_switch),
                    ),
            )
    }

    fn close_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.model_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.model_menu);
        }
    }

    fn toggle_model_menu(&mut self, cx: &mut Context<Self>) {
        if self.model_menu.take_press_was_open() || self.model_menu.is_open() {
            self.close_model_menu(cx);
        } else {
            self.model_menu.open(());
        }
        cx.notify();
    }
}

/// Shared geometry for the right-hand controls (dropdown triggers, key
/// fields), so every row's control lines up on one column.
const CONTROL_WIDTH: f32 = 280.0;
const CONTROL_HEIGHT: f32 = 32.0;
/// Vertical space between groups — wider than the row rhythm so each
/// group reads as its own block without a surface around it.
const GROUP_GAP: f32 = 40.0;

/// A group's caption: the section label (with an optional status pill
/// beside it) over its muted description.
fn group_header(
    theme: &Theme,
    title: &str,
    status: Option<AnyElement>,
    description: &str,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(widgets::section_label(theme, title.to_string()))
                .children(status),
        )
        .child(widgets::row_description(theme, description.to_string()))
}

/// A group's row stack: rows sit directly on the page.
fn group_rows() -> gpui::Div {
    div().mt(px(4.0)).flex().flex_col()
}

/// One row inside [`group_rows`].
fn group_row() -> gpui::Div {
    widgets::flat_row()
}

/// A row's title + description column.
fn row_text(theme: &Theme, title: &str, description: &str) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(widgets::row_title(theme, title.to_string()))
        .child(widgets::row_description(theme, description.to_string()))
}

/// The right-hand control column of a row that commits explicitly: the
/// control with its actions hung right under it, so Save/Remove read as
/// part of the field instead of floating at the group's edge.
fn control_with_actions(control: impl IntoElement, actions: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex_none()
        .w(px(CONTROL_WIDTH))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(control)
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(6.0))
                .children(actions),
        )
}

/// The filled Save button every group shares.
fn save_button(theme: &Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    let hover_theme = theme.clone();
    widgets::ghost_action(theme)
        .id(id)
        .debug_selector(move || id.into())
        .bg(theme.ink(0.06))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_theme.ink(0.10)))
}

/// The quiet destructive Remove action.
fn remove_button(theme: &Theme, id: &'static str) -> gpui::Stateful<gpui::Div> {
    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    widgets::ghost_action(theme)
        .id(id)
        .debug_selector(move || id.into())
        .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
}

/// The provider catalog: the model picker's source.
async fn load_providers(engine: &crate::state::EngineHandle) -> Result<Vec<Provider>, String> {
    let value = engine
        .client()
        .call(methods::LIST_PROVIDERS, serde_json::json!({}))
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Every resolvable model across the configured catalog, one LIST_MODELS
/// per concrete provider variant (the same discovery the model picker uses).
async fn load_model_catalog(
    engine: &crate::state::EngineHandle,
    providers: &[Provider],
) -> Loadable<Vec<Model>> {
    let mut models = Vec::new();
    for provider in configured_providers(providers) {
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

impl Render for GeneralPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match (&self.settings, &self.models) {
            (Loadable::Idle, _) | (Loadable::Loading, _) => {
                popover::skeleton_rows("general-skeleton", &theme, 4, cx.entity_id(), cx)
                    .into_any_element()
            }
            (Loadable::Error(error), _) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            (Loadable::Ready(state), models) => {
                let catalog: &[Model] = models.ready().map(Vec::as_slice).unwrap_or(&[]);
                let rows = model_rows(catalog, self.selected_model.as_deref());

                let model_menu_rows = rows.iter().enumerate().map(|(index, row)| {
                    let row_id = row.id.clone();
                    let selected = row.selected;
                    let title = row.title.clone();
                    let detail = row.detail.clone();
                    popover::menu_row(&theme, selected, format!("title-model-option-{index}"))
                        .id(SharedString::from(format!("title-model-option-{index}")))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            // A pick is a complete edit: commit it right
                            // away instead of parking it behind Save.
                            page.selected_model = row_id.clone();
                            page.close_model_menu(cx);
                            page.save_model(cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().truncate().child(SharedString::from(title)))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(
                                            widgets::ROW_DESCRIPTION_SIZE,
                                        ))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(detail)),
                                ),
                        )
                        .when(row.unresolved, |row| {
                            row.child(widgets::badge(&theme, "unavailable"))
                        })
                        .when(selected, |row| {
                            row.child(
                                crate::icons::icon(crate::icons::CHECK)
                                    .size(px(14.0))
                                    .text_color(theme.accent),
                            )
                        })
                        .into_any_element()
                });
                let model_menu = popover::popover_card(&theme)
                    .id("title-model-scroll")
                    .w(px(360.0))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_model_menu(cx)))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(model_menu_rows)
                    .into_any_element();
                let selected_row = rows.iter().find(|row| row.selected).unwrap_or(&rows[0]);
                let selected_label = SharedString::from(selected_row.title.clone());
                let model_trigger = div()
                    .id("title-model-dropdown")
                    .relative()
                    .flex_none()
                    .w(px(CONTROL_WIDTH))
                    .h(px(CONTROL_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .bg(if self.model_menu.is_open() {
                        theme.ink(0.09)
                    } else {
                        theme.ink(0.05)
                    })
                    .when(!self.model_menu.is_open(), |el| {
                        el.hover(|style| style.bg(crate::theme::ink(0.07)))
                    })
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|page, _, _, _| page.model_menu.note_trigger_press()),
                    )
                    .on_click(cx.listener(|page, _, _, cx| page.toggle_model_menu(cx)))
                    .child(div().flex_1().min_w_0().truncate().child(selected_label))
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .when_some(self.model_menu.get(), |trigger, _| {
                        trigger.child(popover::anchored_menu_below(
                            "title-model-menu",
                            model_menu,
                            self.model_menu.closing_since(),
                        ))
                    });

                let mut card = group_rows()
                    .child(
                        group_row()
                            .child(row_text(
                                &theme,
                                "Title model",
                                "Choose a configured provider model. Disabled keeps the \
                                 fallback title.",
                            ))
                            .child(model_trigger),
                    )
                    .child(
                        group_row()
                            .child(row_text(
                                &theme,
                                "Custom title style",
                                "Style notes for automatic titles — language, tone, naming \
                                 conventions. The core naming rules are built in.",
                            ))
                            .child(
                                div()
                                    .id("custom-title-prompt-toggle")
                                    .flex_none()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|page, _, _, cx| {
                                        page.custom_instruction_enabled =
                                            !page.custom_instruction_enabled;
                                        cx.notify();
                                    }))
                                    .child(widgets::toggle_switch(
                                        &theme,
                                        self.custom_instruction_enabled,
                                    )),
                            ),
                    );
                if self.custom_instruction_enabled {
                    let restore_theme = theme.clone();
                    // The notes editor hangs off the toggle row it belongs to:
                    // no hairline between them.
                    card = card.child(
                        div()
                            .pb(px(14.0))
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(widgets::field_label(&theme, "Title style notes"))
                                    .child(
                                        widgets::ghost_action(&theme)
                                            .id("restore-title-instruction")
                                            .mr(px(-10.0))
                                            .py(px(2.0))
                                            .hover(move |style| {
                                                widgets::ghost_hover(&restore_theme, style)
                                            })
                                            .on_click(cx.listener(|page, _, _, cx| {
                                                page.instruction.update(cx, |input, cx| {
                                                    input.set_text(
                                                        holt_proto::DEFAULT_TITLE_INSTRUCTION,
                                                        cx,
                                                    );
                                                });
                                                cx.notify();
                                            }))
                                            .child("Restore default"),
                                    ),
                            )
                            .child(
                                div()
                                    .px(px(12.0))
                                    .py(px(8.0))
                                    .rounded(px(Theme::CONTROL_RADIUS))
                                    .bg(theme.ink(0.05))
                                    .child(self.instruction.clone()),
                            ),
                    );
                }
                // The model commits on pick; only the style notes are a
                // draft, so the bar shows while they differ from the record.
                let dirty = effective_instruction(
                    self.custom_instruction_enabled,
                    self.instruction.read(cx).text(),
                ) != state.settings.instruction;
                if dirty {
                    card = card.child(
                        div()
                            .pt(px(4.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_end()
                            .gap(px(12.0))
                            .child(widgets::row_description(&theme, "Unsaved changes"))
                            .child(
                                save_button(&theme, "save-title-settings")
                                    .on_click(cx.listener(|page, _, _, cx| page.save(cx)))
                                    .child("Save"),
                            ),
                    );
                }
                let mut column = div().flex().flex_col().child(card);
                if let Some(warning) = state.warning.clone() {
                    column = column.child(widgets::warning_strip(&theme, warning));
                }
                if let Some(error) = self.save_error.clone() {
                    column = column.child(widgets::error_strip(&theme, error));
                }
                column.into_any_element()
            }
        };
        div()
            .id("general-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "General", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Notifications, automatic chat titles, and keys for built-in tools.",
                    ))
                    .child(Self::render_notifications(&theme, cx))
                    .child(
                        div()
                            .mt(px(GROUP_GAP))
                            .child(group_header(
                                &theme,
                                "Automatic chat titles",
                                None,
                                "A new chat keeps its first-line title immediately, then one \
                                 background request to this model can replace it. Your manual \
                                 renames always win.",
                            ))
                            .child(body),
                    )
                    .child(self.web_search.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::{Provider, ProviderId, ProviderVariant};

    /// The Notifications group is device-local: it must render and work even
    /// when the engine-backed automatic-title settings fail to load (here the
    /// state has no engine at all, so the title group lands in its Error
    /// state), and the sound control must be non-interactive — without
    /// changing its stored value — while the master toggle is off.
    #[gpui::test]
    fn notifications_group_survives_title_settings_failure(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| AppState::new());
        let (page, visual) = cx.add_window_view(|_window, cx| GeneralPage::new(state.clone(), cx));
        // Force the first frame: a window only paints after a notify.
        page.update(&mut *visual, |_page, cx| cx.notify());
        visual.run_until_parked();

        let current = |visual: &gpui::VisualTestContext| visual.read(settings::current);

        // Both controls exist despite the title group's load failure.
        let master = visual
            .debug_bounds("completion-notifications-toggle")
            .expect("master toggle renders without engine-backed settings");
        assert!(
            visual
                .debug_bounds("completion-notification-sound-toggle")
                .is_some(),
            "sound toggle renders without engine-backed settings"
        );
        assert!(current(visual).completion_notifications);
        assert!(current(visual).completion_notification_sound);

        // Master off: the sound control goes non-interactive but keeps its
        // stored value.
        visual.simulate_click(master.center(), Default::default());
        visual.run_until_parked();
        assert!(!current(visual).completion_notifications);
        let sound = visual
            .debug_bounds("completion-notification-sound-toggle")
            .expect("sound toggle still renders while disabled");
        visual.simulate_click(sound.center(), Default::default());
        visual.run_until_parked();
        assert!(
            current(visual).completion_notification_sound,
            "a disabled sound control must not change its stored value"
        );

        // Master back on: the sound choice was retained and is editable again.
        let master = visual
            .debug_bounds("completion-notifications-toggle")
            .unwrap();
        visual.simulate_click(master.center(), Default::default());
        visual.run_until_parked();
        assert!(current(visual).completion_notifications);
        assert!(current(visual).completion_notification_sound);
        let sound = visual
            .debug_bounds("completion-notification-sound-toggle")
            .unwrap();
        visual.simulate_click(sound.center(), Default::default());
        visual.run_until_parked();
        assert!(!current(visual).completion_notification_sound);

        // Explicit toggles persist immediately.
        let reloaded = crate::settings::UiSettings::load(dir.path());
        assert!(reloaded.completion_notifications);
        assert!(!reloaded.completion_notification_sound);
    }

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
            context_window: None,
            image_capability: holt_proto::ImageCapability::Unknown,
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

    #[test]
    fn configured_provider_filter_keeps_only_variants_with_credentials() {
        let providers = vec![Provider {
            id: ProviderId::from("acme"),
            name: "Acme".into(),
            abbreviation: "A".into(),
            configured: true,
            variants: vec![
                ProviderVariant {
                    id: ProviderId::from("acme-us"),
                    name: "Acme US".into(),
                    configured: true,
                },
                ProviderVariant {
                    id: ProviderId::from("acme-eu"),
                    name: "Acme EU".into(),
                    configured: false,
                },
            ],
            custom: false,
            logo: None,
        }];
        let configured = configured_providers(&providers);
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].id, ProviderId::from("acme-us"));
    }

    #[test]
    fn empty_custom_instruction_uses_the_default() {
        assert_eq!(
            effective_instruction(true, "  \n"),
            holt_proto::DEFAULT_TITLE_INSTRUCTION
        );
        assert_eq!(
            effective_instruction(false, "custom"),
            holt_proto::DEFAULT_TITLE_INSTRUCTION
        );
        assert_eq!(effective_instruction(true, "custom"), "custom");
    }
}
