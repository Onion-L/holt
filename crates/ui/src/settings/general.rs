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
use holt_proto::{GoalSettingsState, Model, Provider, TitleSettingsState};
use holt_rpc::methods;

mod web_search;

use crate::{
    composer::ComposerInput,
    i18n::t,
    popover::{self, Loadable, Popup},
    settings::{self, SavePolicy, widgets},
    state::AppState,
    theme::Theme,
};

/// One rendered model-choice row. `id: None` is the default row — what
/// "no selection" means is per-feature (Disabled for titles, the chat's
/// own model for the goal verifier).
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

/// Assemble the model-choice rows: the feature's default first, then every
/// resolvable model in catalog order, with the current selection marked. A
/// stored selection missing from the catalog is appended as an unresolved
/// row.
fn model_rows(models: &[Model], selected: Option<&str>, default: ModelRow) -> Vec<ModelRow> {
    let mut rows = vec![ModelRow {
        selected: selected.is_none(),
        ..default
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
            detail: t!("settings_general_appearance.general.model_rows.not_in_catalog").into(),
            selected: true,
            unresolved: true,
        });
    }
    rows
}

/// The title picker's default row: no model, no automatic titles.
fn title_default_row() -> ModelRow {
    ModelRow {
        id: None,
        title: t!("settings_general_appearance.general.model_rows.disabled").into(),
        detail: t!("settings_general_appearance.general.model_rows.disabled_description").into(),
        selected: false,
        unresolved: false,
    }
}

/// The goal-verifier picker's default row: no separate judge, every
/// chat's goal loop verifies on its own model.
fn verifier_default_row() -> ModelRow {
    ModelRow {
        id: None,
        title: t!("settings_general_appearance.general.model_rows.chat_model").into(),
        detail: t!("settings_general_appearance.general.model_rows.chat_model_description").into(),
        selected: false,
        unresolved: false,
    }
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

/// Which settings record a model picker edits — the General page has one
/// picker per engine-owned model setting, sharing the menu machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelTarget {
    Title,
    Verifier,
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
    /// The goal-verifier record (ADR-goal-mode-verified-loop-at-turn-settle follow-up): its own read/save
    /// lifecycle, warning strip included, independent of the title group.
    goal_settings: Loadable<GoalSettingsState>,
    verifier_model: Option<String>,
    verifier_menu: Popup<()>,
    verifier_save_error: Option<String>,
    task: Option<Task<()>>,
    /// The Web search group (web-tools ticket 07).
    web_search: Entity<web_search::WebSearchGroup>,
    /// The interface-language picker (`i18n`): device-local, no engine.
    language_menu: Popup<()>,
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
            instruction: cx.new(|cx| {
                ComposerInput::new(
                    t!("settings_general_appearance.general.instruction_placeholder"),
                    cx,
                )
            }),
            save_error: None,
            goal_settings: Loadable::Idle,
            verifier_model: None,
            verifier_menu: Popup::default(),
            verifier_save_error: None,
            task: None,
            web_search,
            language_menu: Popup::default(),
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
            self.settings = Loadable::Error(crate::state::engine_not_connected().into());
            self.models = Loadable::Error(crate::state::engine_not_connected().into());
            self.goal_settings = Loadable::Error(crate::state::engine_not_connected().into());
            return;
        };
        self.settings = Loadable::Loading;
        self.models = Loadable::Loading;
        self.goal_settings = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let settings_result = engine
                .client()
                .call(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
                .await;
            let goal_result = engine
                .client()
                .call(methods::GET_GOAL_SETTINGS, serde_json::json!({}))
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
                            t!("settings_general_appearance.general.errors.titles_unavailable")
                                .into(),
                        );
                    }
                    Err(error) => page.settings = Loadable::Error(error.to_string()),
                }
                match goal_result {
                    Ok(value) => match serde_json::from_value::<GoalSettingsState>(value) {
                        Ok(state) => {
                            page.verifier_model = state.settings.model_id.clone();
                            page.goal_settings = Loadable::Ready(state);
                        }
                        Err(error) => page.goal_settings = Loadable::Error(error.to_string()),
                    },
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        page.goal_settings = Loadable::Error(
                            t!("settings_general_appearance.general.errors.verifier_unavailable")
                                .into(),
                        );
                    }
                    Err(error) => page.goal_settings = Loadable::Error(error.to_string()),
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
            self.save_error = Some(crate::state::engine_not_connected().into());
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
                &t!("settings_general_appearance.general.notifications.title"),
                None,
                &t!("settings_general_appearance.general.notifications.description"),
            ))
            .child(
                group_rows()
                    .child(
                        group_row()
                            .child(row_text(
                                theme,
                                &t!("settings_general_appearance.general.notifications.completion"),
                                &t!("settings_general_appearance.general.notifications.completion_description"),
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
                                &t!("settings_general_appearance.general.notifications.sound"),
                                &t!("settings_general_appearance.general.notifications.sound_description"),
                            ))
                            .child(sound_switch),
                    ),
            )
    }

    /// The Language group (`i18n`): device-local like Notifications, with no
    /// engine behind it, so it renders whatever the title group's load did.
    /// A pick applies through `i18n::set_language` — persisted, every window
    /// repainted, native menus rebuilt — so this page re-renders in the new
    /// language on the same frame.
    fn render_language(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .mt(px(GROUP_GAP))
            .child(group_header(
                theme,
                &t!("settings_general_appearance.language.group_title"),
                None,
                &t!("settings_general_appearance.language.group_description"),
            ))
            .child(
                group_rows().child(
                    group_row()
                        .child(row_text(
                            theme,
                            &t!("settings_general_appearance.language.row_title"),
                            &t!("settings_general_appearance.language.row_description"),
                        ))
                        .child(self.render_language_picker(theme, cx)),
                ),
            )
    }

    fn close_language_menu(&mut self, cx: &mut Context<Self>) {
        if self.language_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.language_menu);
        }
    }

    fn toggle_language_menu(&mut self, cx: &mut Context<Self>) {
        if self.language_menu.take_press_was_open() || self.language_menu.is_open() {
            self.close_language_menu(cx);
        } else {
            self.language_menu.open(());
        }
        cx.notify();
    }

    /// The interface-language picker: "Follow system" first, then each
    /// shipped language under its autonym (`i18n::LANGUAGES` — constants,
    /// never catalog strings, so the picker stays findable in either
    /// language). Same trigger-plus-anchored-menu shape as the model
    /// pickers.
    fn render_language_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let preference = cx
            .try_global::<crate::i18n::I18nState>()
            .and_then(|state| state.preference.clone());
        // (label, choice, selected) — `None` is "follow the OS".
        let mut choices: Vec<(SharedString, Option<&'static str>, bool)> = vec![(
            t!("settings_general_appearance.language.follow_system").into(),
            None,
            preference.is_none(),
        )];
        choices.extend(crate::i18n::LANGUAGES.iter().map(|language| {
            (
                SharedString::from(language.name),
                Some(language.code),
                preference.as_deref() == Some(language.code),
            )
        }));
        let selected_label = choices
            .iter()
            .find(|(_, _, selected)| *selected)
            .map(|(label, _, _)| label.clone())
            .unwrap_or_else(|| choices[0].0.clone());
        let rows = choices
            .iter()
            .enumerate()
            .map(|(index, (label, choice, selected))| {
                let option_id: SharedString = format!("interface-language-option-{index}").into();
                let debug_id = option_id.clone();
                let choice = *choice;
                popover::menu_row(theme, *selected, option_id.clone())
                    .id(option_id)
                    .debug_selector(move || debug_id.to_string())
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.close_language_menu(cx);
                        crate::i18n::set_language(choice, cx);
                        cx.notify();
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                    .when(*selected, |row| {
                        row.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(14.0))
                                .text_color(theme.accent),
                        )
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let menu = popover::popover_card(theme)
            .id("interface-language-scroll")
            .w(px(CONTROL_WIDTH))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_language_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows)
            .into_any_element();
        let open = self.language_menu.is_open();
        div()
            .id("interface-language-dropdown")
            .debug_selector(|| "interface-language-dropdown".into())
            .relative()
            .flex_none()
            .w(px(CONTROL_WIDTH))
            .h(px(CONTROL_HEIGHT))
            .px(px(10.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(if open {
                theme.ink(0.09)
            } else {
                theme.ink(0.05)
            })
            .when(!open, |el| {
                el.hover(|style| style.bg(crate::theme::ink(0.07)))
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, _, _| page.language_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|page, _, _, cx| page.toggle_language_menu(cx)))
            .child(div().flex_1().min_w_0().truncate().child(selected_label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.language_menu.get().cloned(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "interface-language-menu",
                    menu,
                    self.language_menu.closing_since(),
                ))
            })
            .into_any_element()
    }

    fn menu_for(&mut self, target: ModelTarget) -> &mut Popup<()> {
        match target {
            ModelTarget::Title => &mut self.model_menu,
            ModelTarget::Verifier => &mut self.verifier_menu,
        }
    }

    /// The read-side of [`Self::menu_for`] for render paths, where an
    /// immutable borrow of the popup's state is all the picker needs.
    fn menu(&self, target: ModelTarget) -> &Popup<()> {
        match target {
            ModelTarget::Title => &self.model_menu,
            ModelTarget::Verifier => &self.verifier_menu,
        }
    }

    fn close_model_menu(&mut self, target: ModelTarget, cx: &mut Context<Self>) {
        if self.menu_for(target).begin_close() {
            popover::reap_popup(cx, move |page| page.menu_for(target));
        }
    }

    fn toggle_model_menu(&mut self, target: ModelTarget, cx: &mut Context<Self>) {
        let menu = self.menu_for(target);
        if menu.take_press_was_open() || menu.is_open() {
            self.close_model_menu(target, cx);
        } else {
            menu.open(());
        }
        cx.notify();
    }

    /// A pick is a complete edit: commit it right away instead of parking
    /// it behind a Save (both records normalize on save, so the reply is
    /// the truth the page shows).
    fn pick_model(&mut self, target: ModelTarget, id: Option<String>, cx: &mut Context<Self>) {
        match target {
            ModelTarget::Title => {
                self.selected_model = id;
                self.save_model(cx);
            }
            ModelTarget::Verifier => {
                self.verifier_model = id;
                self.save_verifier_model(cx);
            }
        }
    }

    /// Commit a verifier pick on its own: `SaveGoalSettings` with the
    /// chosen model (or the default), the reply refreshing the warning.
    fn save_verifier_model(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.verifier_save_error = Some(crate::state::engine_not_connected().into());
            cx.notify();
            return;
        };
        let model_id = self.verifier_model.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SAVE_GOAL_SETTINGS,
                    serde_json::json!({ "modelId": model_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<GoalSettingsState>(value) {
                        Ok(state) => {
                            page.verifier_model = state.settings.model_id.clone();
                            page.goal_settings = Loadable::Ready(state);
                            page.verifier_save_error = None;
                        }
                        Err(error) => page.verifier_save_error = Some(error.to_string()),
                    },
                    Err(error) => page.verifier_save_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }
    /// The shared model picker: trigger button plus its anchored menu,
    /// parameterized by which record it edits. The menu lists the feature's
    /// default row first, then the catalog.
    #[allow(clippy::too_many_arguments)]
    fn render_model_picker(
        &self,
        target: ModelTarget,
        rows: &[ModelRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (trigger_id, scroll_id, menu_id, option_prefix) = match target {
            ModelTarget::Title => (
                "title-model-dropdown",
                "title-model-scroll",
                "title-model-menu",
                "title-model-option",
            ),
            ModelTarget::Verifier => (
                "goal-verifier-dropdown",
                "goal-verifier-scroll",
                "goal-verifier-menu",
                "goal-verifier-option",
            ),
        };
        let model_menu_rows = rows.iter().enumerate().map(|(index, row)| {
            let row_id = row.id.clone();
            let selected = row.selected;
            let title = row.title.clone();
            let detail = row.detail.clone();
            let option_id: SharedString = format!("{option_prefix}-{index}").into();
            let debug_id = option_id.clone();
            popover::menu_row(theme, selected, option_id.clone())
                .id(option_id.clone())
                .debug_selector(move || debug_id.to_string())
                .on_click(cx.listener(move |page, _, _, cx| {
                    cx.stop_propagation();
                    page.pick_model(target, row_id.clone(), cx);
                    page.close_model_menu(target, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().truncate().child(title))
                        .child(
                            div()
                                .truncate()
                                .text_size(crate::typography::ui_rems(
                                    widgets::ROW_DESCRIPTION_SIZE,
                                ))
                                .text_color(theme.text_muted)
                                .child(detail),
                        ),
                )
                .when(row.unresolved, |row| {
                    row.child(widgets::badge(
                        theme,
                        t!("settings_general_appearance.general.model_rows.unavailable"),
                    ))
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
        let menu = popover::popover_card(theme)
            .id(scroll_id)
            .w(px(360.0))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(move |page, _, _, cx| page.close_model_menu(target, cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(model_menu_rows)
            .into_any_element();
        let selected_row = rows.iter().find(|row| row.selected).unwrap_or(&rows[0]);
        let selected_label = SharedString::from(selected_row.title.clone());
        let trigger_debug_id = trigger_id;
        let trigger = div()
            .id(trigger_id)
            .debug_selector(move || trigger_debug_id.to_string())
            .relative()
            .flex_none()
            .w(px(CONTROL_WIDTH))
            .h(px(CONTROL_HEIGHT))
            .px(px(10.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(if self.menu(target).is_open() {
                theme.ink(0.09)
            } else {
                theme.ink(0.05)
            })
            .when(!self.menu(target).is_open(), |el| {
                el.hover(|style| style.bg(crate::theme::ink(0.07)))
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |page, _, _, _| page.menu_for(target).note_trigger_press()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |page, _, _, _| page.menu_for(target).note_trigger_press()),
            )
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_model_menu(target, cx)))
            .child(div().flex_1().min_w_0().truncate().child(selected_label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.menu(target).get().cloned(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    menu_id,
                    menu,
                    self.menu(target).closing_since(),
                ))
            });
        trigger.into_any_element()
    }

    /// The goal-verifier group: one picker over the engine-owned record.
    /// Independent of the title group — it renders its own loading, error,
    /// and warning states so one group failing never blanks the other.
    fn render_goal_verifier(
        &self,
        models: Option<&[Model]>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut group = div().mt(px(GROUP_GAP)).child(group_header(
            theme,
            &t!("settings_general_appearance.general.groups.verifier_title"),
            None,
            &t!("settings_general_appearance.general.groups.verifier_description"),
        ));
        let body = match (&self.goal_settings, models) {
            (Loadable::Idle, _) | (Loadable::Loading, _) => {
                popover::skeleton_rows("goal-verifier-skeleton", theme, 1, cx.entity_id(), cx)
                    .into_any_element()
            }
            (Loadable::Error(error), _) => {
                widgets::error_strip(theme, error.clone()).into_any_element()
            }
            (Loadable::Ready(state), models) => {
                let Some(catalog) = models else {
                    // The record loaded but the catalog didn't (no
                    // configured providers): the picker has nothing to
                    // offer beyond the default — say so instead of a
                    // skeleton that never resolves.
                    return widgets::error_strip(
                        theme,
                        t!("settings_general_appearance.general.groups.no_providers"),
                    )
                    .into_any_element();
                };
                let rows = model_rows(
                    catalog,
                    self.verifier_model.as_deref(),
                    verifier_default_row(),
                );
                let picker = self.render_model_picker(ModelTarget::Verifier, &rows, theme, cx);
                div()
                    .mt(px(4.0))
                    .flex()
                    .flex_col()
                    .child(
                        group_row()
                            .child(row_text(
                                theme,
                                &t!(
                                    "settings_general_appearance.general.groups.verifier_row_title"
                                ),
                                &t!("settings_general_appearance.general.groups.verifier_row_description"),
                            ))
                            .child(picker),
                    )
                    .when_some(state.warning.clone(), |el, warning| {
                        el.child(widgets::warning_strip(theme, warning))
                    })
                    .when_some(self.verifier_save_error.clone(), |el, error| {
                        el.child(widgets::error_strip(theme, error))
                    })
                    .into_any_element()
            }
        };
        group = group.child(body);
        group.into_any_element()
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
                let rows = model_rows(catalog, self.selected_model.as_deref(), title_default_row());
                let model_trigger = self.render_model_picker(ModelTarget::Title, &rows, &theme, cx);

                let mut card = group_rows()
                    .child(
                        group_row()
                            .child(row_text(
                                &theme,
                                &t!("settings_general_appearance.general.titles.title_model"),
                                &t!("settings_general_appearance.general.titles.title_model_description"),
                            ))
                            .child(model_trigger),
                    )
                    .child(
                        group_row()
                            .child(row_text(
                                &theme,
                                &t!("settings_general_appearance.general.titles.custom_style"),
                                &t!("settings_general_appearance.general.titles.custom_style_description"),
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
                                    .child(widgets::field_label(
                                        &theme,
                                        t!(
                                            "settings_general_appearance.general.titles.notes_label"
                                        ),
                                    ))
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
                                            .child(t!(
                                                "settings_general_appearance.general.titles.\
                                                 restore_default"
                                            )),
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
                            .child(widgets::row_description(
                                &theme,
                                t!("settings_general_appearance.general.titles.unsaved"),
                            ))
                            .child(
                                save_button(&theme, "save-title-settings")
                                    .on_click(cx.listener(|page, _, _, cx| page.save(cx)))
                                    .child(t!("settings_general_appearance.general.titles.save")),
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
                    .child(widgets::page_header(
                        &theme,
                        &t!("settings_general_appearance.general.title"),
                        None,
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        t!("settings_general_appearance.general.subtitle"),
                    ))
                    .child(self.render_language(&theme, cx))
                    .child(Self::render_notifications(&theme, cx))
                    .child(
                        div()
                            .mt(px(GROUP_GAP))
                            .child(group_header(
                                &theme,
                                &t!("settings_general_appearance.general.groups.titles_title"),
                                None,
                                &t!(
                                    "settings_general_appearance.general.groups.titles_description"
                                ),
                            ))
                            .child(body),
                    )
                    .child(self.render_goal_verifier(
                        self.models.ready().map(Vec::as_slice),
                        &theme,
                        cx,
                    ))
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
        let rows = model_rows(
            &[model("openai/gpt-5.4", "GPT-5.4")],
            None,
            title_default_row(),
        );
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
        let rows = model_rows(&models, Some("openai/gpt-5.4"), title_default_row());
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].selected);
        assert!(!rows[1].selected);
        assert!(rows[2].selected);
        assert!(!rows.iter().any(|row| row.unresolved));
    }

    #[test]
    fn a_selection_missing_from_the_catalog_stays_visible_as_unresolved() {
        let rows = model_rows(
            &[model("openai/gpt-5.4", "GPT-5.4")],
            Some("old/provider"),
            title_default_row(),
        );
        let unresolved = rows.iter().find(|row| row.unresolved).unwrap();
        assert_eq!(unresolved.id.as_deref(), Some("old/provider"));
        assert!(unresolved.selected);
        assert!(rows.iter().filter(|row| row.selected).count() == 1);
    }

    #[test]
    fn an_empty_catalog_still_offers_disabled() {
        let rows = model_rows(&[], None, title_default_row());
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

    /// The Goal verifier group drives its own RPC: the picker lists the
    /// chat-model default plus the catalog, and a pick commits through
    /// `SaveGoalSettings` immediately (the title picker's commit-on-pick
    /// precedent).
    #[gpui::test]
    fn the_verifier_pick_commits_through_save_goal_settings(cx: &mut gpui::TestAppContext) {
        use std::sync::{Arc, Mutex};

        use crate::state::AppState;

        /// Serves the General page's reads and records the verifier save.
        struct GeneralEngine {
            saves: Mutex<Vec<serde_json::Value>>,
        }

        #[async_trait::async_trait]
        impl holt_rpc::RpcService for GeneralEngine {
            async fn handle(
                &self,
                method: &str,
                params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                match method {
                    methods::GET_TITLE_SETTINGS => holt_rpc::RpcReply::value(&serde_json::json!({
                        "settings": { "modelId": null, "instruction": "name it" }
                    })),
                    methods::GET_GOAL_SETTINGS => holt_rpc::RpcReply::value(
                        &serde_json::json!({ "settings": { "modelId": null } }),
                    ),
                    methods::LIST_PROVIDERS => holt_rpc::RpcReply::value(&serde_json::json!([{
                        "id": "openai", "name": "OpenAI", "abbreviation": "OA",
                        "configured": true, "custom": false,
                        "variants": [
                            { "id": "openai", "name": "OpenAI", "configured": true }
                        ],
                    }])),
                    methods::LIST_MODELS => holt_rpc::RpcReply::value(&serde_json::json!([
                        {
                            "id": "openai/gpt-5.4", "provider": "openai",
                            "label": "GPT-5.4", "custom": false,
                        },
                        {
                            "id": "openai/gpt-5.4-mini", "provider": "openai",
                            "label": "GPT-5.4 Mini", "custom": false,
                        },
                    ])),
                    methods::SAVE_GOAL_SETTINGS => {
                        self.saves.lock().unwrap().push(params.clone());
                        holt_rpc::RpcReply::value(&serde_json::json!({ "settings": params }))
                    }
                    _ => Err(holt_rpc::RpcError::UnknownMethod(method.to_string())),
                }
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cx.update(|cx| cx.set_global(Theme::default()));
        let engine = Arc::new(GeneralEngine {
            saves: Mutex::new(Vec::new()),
        });
        let state = cx.new(|_| AppState::new());
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (page, visual) = cx.add_window_view(|_window, cx| GeneralPage::new(state.clone(), cx));
        page.update(&mut *visual, |_page, cx| cx.notify());
        for _ in 0..8 {
            runtime.block_on(async { tokio::task::yield_now().await });
            visual.run_until_parked();
        }

        // The trigger shows the default (the chat's model), the menu lists
        // the default row plus both catalog models.
        let trigger = visual
            .debug_bounds("goal-verifier-dropdown")
            .expect("the verifier picker renders");
        visual.simulate_click(trigger.center(), Default::default());
        visual.run_until_parked();
        let option = visual
            .debug_bounds("goal-verifier-option-2")
            .expect("the second catalog model is offered");
        visual.simulate_click(option.center(), Default::default());
        for _ in 0..8 {
            runtime.block_on(async { tokio::task::yield_now().await });
            visual.run_until_parked();
        }

        let saves = engine.saves.lock().unwrap();
        assert_eq!(saves.len(), 1, "the pick commits exactly one save");
        assert_eq!(saves[0]["modelId"], "openai/gpt-5.4-mini");
    }
}
