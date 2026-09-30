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
    AnyElement, App, Context, Entity, IntoElement, MouseButton, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_proto::{JevSettingsState, Model, Provider, TitleSettingsState};
use holt_rpc::methods;

mod web_search;

use crate::{
    composer::{ComposerInput, ComposerInputEvent},
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

/// What a stored-key field (Web search, Jev) currently shows.
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
    /// The engine-owned Jev record (ADR-0027): the user's own TypeSafe
    /// key, mounted by future Jev-powered features.
    jev: Loadable<JevSettingsState>,
    jev_key: Entity<ComposerInput>,
    /// The raw stored key the field currently shows, fetched by
    /// `RevealJevKey`; `None` while it shows the masked display or a draft.
    jev_revealed_key: Option<String>,
    /// Draft-only projection: the eye hides the key the user is typing.
    jev_draft_concealed: bool,
    jev_error: Option<String>,
    /// Re-derives the Jev key field's projection on every edit: a pasted
    /// key is concealed the moment it stops being the masked display.
    _jev_key_events: Subscription,
}

impl GeneralPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let jev_key = cx.new(|cx| ComposerInput::new_secret("TypeSafe API key", cx));
        let jev_key_events = cx.subscribe(
            &jev_key,
            |page: &mut Self, _, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    page.sync_jev_mask(cx);
                }
            },
        );
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
            jev: Loadable::Idle,
            jev_key,
            jev_revealed_key: None,
            jev_draft_concealed: true,
            jev_error: None,
            _jev_key_events: jev_key_events,
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
            self.jev = Loadable::Error("Engine not connected".into());
            return;
        };
        self.settings = Loadable::Loading;
        self.models = Loadable::Loading;
        self.jev = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let settings_result = engine
                .client()
                .call(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
                .await;
            let models_result = match load_providers(&engine).await {
                Ok(providers) => load_model_catalog(&engine, &providers).await,
                Err(error) => Loadable::Error(error),
            };
            let jev_result = engine
                .client()
                .call(methods::GET_JEV_SETTINGS, serde_json::json!({}))
                .await;
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
                match jev_result {
                    Ok(value) => match serde_json::from_value::<JevSettingsState>(value) {
                        Ok(state) => page.apply_jev_state(state, cx),
                        Err(error) => page.jev = Loadable::Error(error.to_string()),
                    },
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        page.jev = Loadable::Error(
                            "Jev settings aren't available — the engine doesn't support them yet"
                                .into(),
                        );
                    }
                    Err(error) => page.jev = Loadable::Error(error.to_string()),
                }
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

    /// The Jev field's projection state — the same three-state machine as
    /// the web-search key, tracked against the Jev record's masked display.
    fn jev_key_field_state(&self, cx: &App) -> KeyField {
        let text = self.jev_key.read(cx).text();
        if self.jev_revealed_key.as_deref() == Some(text) {
            return KeyField::Revealed;
        }
        let stored = self
            .jev
            .ready()
            .and_then(|state| state.api_key_masked.as_deref());
        if stored == Some(text) {
            KeyField::Stored
        } else {
            KeyField::Draft
        }
    }

    fn sync_jev_mask(&mut self, cx: &mut Context<Self>) {
        let masked = self.jev_key_field_state(cx) == KeyField::Draft && self.jev_draft_concealed;
        self.jev_key
            .update(cx, |input, cx| input.set_masked(masked, cx));
    }

    /// Echo a Jev reply (read, save, or remove) into the group's editable
    /// state — the stored truth is what the page shows.
    fn apply_jev_state(&mut self, state: JevSettingsState, cx: &mut Context<Self>) {
        let masked = state.api_key_masked.clone().unwrap_or_default();
        self.jev = Loadable::Ready(state);
        self.jev_revealed_key = None;
        self.jev_draft_concealed = true;
        self.jev_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
    }

    /// The eye button on the Jev key field: stored keys reveal and conceal
    /// through `RevealJevKey`; a draft is only a projection flip.
    fn toggle_jev_key(&mut self, cx: &mut Context<Self>) {
        match self.jev_key_field_state(cx) {
            KeyField::Stored => self.reveal_jev_key(cx),
            KeyField::Revealed => self.restore_masked_jev_key(cx),
            KeyField::Draft => {
                self.jev_draft_concealed = !self.jev_draft_concealed;
                self.sync_jev_mask(cx);
                cx.notify();
            }
        }
    }

    fn reveal_jev_key(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.jev_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::REVEAL_JEV_KEY, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match value
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .filter(|key| !key.is_empty())
                    {
                        Some(key) => {
                            page.jev_revealed_key = Some(key.to_string());
                            page.jev_key.update(cx, |input, cx| {
                                input.set_masked(false, cx);
                                input.set_text(key, cx);
                            });
                            page.jev_error = None;
                        }
                        None => {
                            page.jev_error = Some("No API key is stored".into());
                        }
                    },
                    Err(error) => page.jev_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Conceal again: put the engine's masked key back in the field.
    fn restore_masked_jev_key(&mut self, cx: &mut Context<Self>) {
        let masked = self
            .jev
            .ready()
            .and_then(|state| state.api_key_masked.clone())
            .unwrap_or_default();
        self.jev_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
        self.jev_revealed_key = None;
        self.jev_draft_concealed = true;
        cx.notify();
    }

    /// Save the key field's content. An untouched field holds the engine's
    /// masked display, never a usable key: the stored key is re-read through
    /// `RevealJevKey` and re-saved — writing the masked display would
    /// corrupt the record.
    fn save_jev(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.jev_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let draft = self.jev_key.read(cx).text().to_string();
        let untouched = self.jev_key_field_state(cx) == KeyField::Stored;
        self.task = Some(cx.spawn(async move |this, cx| {
            let key = if untouched {
                match engine
                    .client()
                    .call(methods::REVEAL_JEV_KEY, serde_json::json!({}))
                    .await
                {
                    Ok(value) => value
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    Err(error) => {
                        this.update(cx, |page, cx| {
                            page.jev_error = Some(error.to_string());
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            } else {
                draft
            };
            if key.trim().is_empty() {
                this.update(cx, |page, cx| {
                    page.jev_error = Some("Enter an API key".into());
                    cx.notify();
                })
                .ok();
                return;
            }
            let result = engine
                .client()
                .call(
                    methods::SAVE_JEV_SETTINGS,
                    serde_json::json!({ "apiKey": key }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<JevSettingsState>(value) {
                        Ok(state) => {
                            page.apply_jev_state(state, cx);
                            page.jev_error = None;
                        }
                        Err(error) => page.jev_error = Some(error.to_string()),
                    },
                    Err(error) => page.jev_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Clear the Jev record — the unconfigured state is no file at all,
    /// and the tier goes gray again.
    fn remove_jev(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.jev_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::REMOVE_JEV_SETTINGS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.jev_error = None;
                        page.apply_jev_state(
                            JevSettingsState {
                                api_key_masked: None,
                            },
                            cx,
                        );
                    }
                    Err(error) => page.jev_error = Some(error.to_string()),
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

    /// The Jev connection group (ADR-0027): the TypeSafe key future
    /// Jev-powered features mount from, with the same reveal affordance
    /// and Save/Remove actions as Web search. No feature consumes it yet;
    /// the group's state is independent of its neighbors.
    fn render_jev(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.jev {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("jev-skeleton", theme, 1, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => widgets::error_strip(theme, error.clone())
                .id("jev-unavailable")
                .debug_selector(|| "jev-unavailable".into())
                .into_any_element(),
            Loadable::Ready(state) => {
                let configured = state.api_key_masked.is_some();
                let mut actions = Vec::new();
                if configured {
                    actions.push(
                        remove_button(theme, "remove-jev")
                            .on_click(cx.listener(|page, _, _, cx| page.remove_jev(cx)))
                            .child("Remove")
                            .into_any_element(),
                    );
                }
                actions.push(
                    save_button(theme, "save-jev")
                        .on_click(cx.listener(|page, _, _, cx| page.save_jev(cx)))
                        .child("Save")
                        .into_any_element(),
                );
                let card = group_rows().child(
                    group_row()
                        .items_start()
                        .child(row_text(
                            theme,
                            "API key",
                            "Stored on this device, separate from your provider keys.",
                        ))
                        .child(control_with_actions(
                            jev_key_field(
                                theme,
                                self.jev_key.clone(),
                                self.jev_key_field_state(cx),
                                self.jev_draft_concealed,
                                cx,
                            ),
                            actions,
                        )),
                );
                let mut column = div().flex().flex_col().child(card);
                if let Some(error) = self.jev_error.clone() {
                    column = column.child(
                        widgets::error_strip(theme, error)
                            .id("jev-error")
                            .debug_selector(|| "jev-error".into()),
                    );
                }
                column.into_any_element()
            }
        };
        let status = self
            .jev
            .ready()
            .map(|state| status_pill(theme, state.api_key_masked.is_some(), "jev-unconfigured"));
        div()
            .id("jev-group")
            .mt(px(GROUP_GAP))
            .child(group_header(
                theme,
                "Jev (TypeSafe)",
                status,
                "Your own TypeSafe API key. Jev-powered features use it as they \
                 arrive; the key is stored and ready either way.",
            ))
            .child(body)
            .into_any_element()
    }
}

/// Shared geometry for the right-hand controls (dropdown triggers, key
/// fields), so every row's control lines up on one column.
const CONTROL_WIDTH: f32 = 280.0;
const CONTROL_HEIGHT: f32 = 32.0;
/// Vertical space between groups — wider than the row rhythm so each
/// group reads as its own block without a surface around it.
const GROUP_GAP: f32 = 40.0;
/// No Jev-powered feature ships yet, so the key group stays hidden rather
/// than suggest the key does something. The load/save/remove plumbing is
/// kept; flip this once a feature consumes the key.
const JEV_GROUP_VISIBLE: bool = false;

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

/// The status pill beside a keyed group's title.
fn status_pill(theme: &Theme, configured: bool, id: &'static str) -> AnyElement {
    if configured {
        widgets::badge_active(theme, "Configured").into_any_element()
    } else {
        div()
            .id(id)
            .debug_selector(move || id.into())
            .flex_none()
            .px(px(8.0))
            .py(px(2.0))
            .rounded_full()
            .bg(theme.ink(0.06))
            .text_size(crate::typography::ui_rems(10.5))
            .text_color(theme.text_muted)
            .child("Not configured")
            .into_any_element()
    }
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

/// The API-key field for the Jev group — the same affordance as the
/// Web search key field, with its own ids and toggle.
fn jev_key_field(
    theme: &Theme,
    input: Entity<ComposerInput>,
    field: KeyField,
    draft_concealed: bool,
    cx: &mut Context<GeneralPage>,
) -> impl IntoElement {
    let hover_theme = theme.clone();
    let showing_plain =
        field == KeyField::Revealed || (field == KeyField::Draft && !draft_concealed);
    div()
        .id("jev-key-field")
        .debug_selector(|| "jev-key-field".into())
        .flex_none()
        .w(px(CONTROL_WIDTH))
        .h(px(CONTROL_HEIGHT))
        .pl(px(10.0))
        .pr(px(2.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Theme::CONTROL_RADIUS))
        .bg(theme.ink(0.05))
        .child(div().flex_1().min_w_0().child(input))
        .child(
            widgets::ghost_action(theme)
                .flex_none()
                .id("toggle-jev-key")
                .debug_selector(|| "toggle-jev-key".into())
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(cx.listener(|page, _, _, cx| page.toggle_jev_key(cx)))
                .child(
                    crate::icons::icon(if showing_plain {
                        crate::icons::EYE_SLASH
                    } else {
                        crate::icons::EYE
                    })
                    .size(px(15.0))
                    .text_color(theme.text_muted),
                ),
        )
        .into_any_element()
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
                    .child(self.web_search.clone())
                    .when(JEV_GROUP_VISIBLE, |page| {
                        page.child(self.render_jev(&theme, cx))
                    }),
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

    /// The engine's mask: first and last four characters, nothing for short
    /// keys.
    fn masked(key: &str) -> String {
        let chars: Vec<char> = key.chars().collect();
        if chars.len() <= 8 {
            return "…".into();
        }
        format!(
            "{}…{}",
            chars[..4].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    }

    /// The Jev group's engine seam plus the title/provider reads the page
    /// performs on load. Everything else (Web search included) is version
    /// skew — the Web search group has its own tests.
    struct FakeJevEngine {
        state: std::sync::Mutex<JevSettingsState>,
        key: std::sync::Mutex<Option<String>>,
        saved: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl holt_rpc::RpcService for FakeJevEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
            use holt_rpc::{RpcError, RpcReply};
            match method {
                methods::GET_TITLE_SETTINGS => RpcReply::value(&serde_json::json!({
                    "settings": {
                        "modelId": null,
                        "instruction": holt_proto::DEFAULT_TITLE_INSTRUCTION,
                    },
                })),
                methods::LIST_PROVIDERS => RpcReply::value(&serde_json::json!([])),
                methods::GET_JEV_SETTINGS => RpcReply::value(&*self.state.lock().unwrap()),
                methods::REVEAL_JEV_KEY => RpcReply::value(&serde_json::json!({
                    "key": self.key.lock().unwrap().clone(),
                })),
                methods::SAVE_JEV_SETTINGS => {
                    let api_key = params
                        .get("apiKey")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if api_key.trim().is_empty() {
                        return Err(RpcError::BadParams("apiKey is required".into()));
                    }
                    self.saved.lock().unwrap().push(params);
                    *self.key.lock().unwrap() = Some(api_key.clone());
                    *self.state.lock().unwrap() = JevSettingsState {
                        api_key_masked: Some(masked(&api_key)),
                    };
                    RpcReply::value(&*self.state.lock().unwrap())
                }
                methods::REMOVE_JEV_SETTINGS => {
                    *self.key.lock().unwrap() = None;
                    *self.state.lock().unwrap() = JevSettingsState {
                        api_key_masked: None,
                    };
                    RpcReply::value(&serde_json::json!({}))
                }
                _ => Err(RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    struct JevHarness<'a> {
        page: Entity<GeneralPage>,
        visual: &'a mut gpui::VisualTestContext,
        engine: std::sync::Arc<FakeJevEngine>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl JevHarness<'_> {
        /// Drive RPC dispatch a few rounds (test thread), then the gpui
        /// foreground executor — the notifications harness' pump.
        fn pump(&self) {
            for _ in 0..6 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                self.visual.run_until_parked();
            }
        }
    }

    fn jev_harness<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: JevSettingsState,
        key: Option<&str>,
    ) -> JevHarness<'a> {
        let engine = std::sync::Arc::new(FakeJevEngine {
            state: std::sync::Mutex::new(state),
            key: std::sync::Mutex::new(key.map(str::to_string)),
            saved: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let app_state = cx.new(|_| AppState::new());
        // `memory_client` spawns its dispatch loop with `tokio::spawn`,
        // which needs a runtime context on this thread.
        let client = {
            let _guard = runtime.enter();
            holt_rpc::memory_client(engine.clone())
        };
        app_state.update(cx, |state, cx| state.attach_test_engine(client, cx));
        let (page, visual) =
            cx.add_window_view(|_window, cx| GeneralPage::new(app_state.clone(), cx));
        let harness = JevHarness {
            page,
            visual,
            engine,
            runtime,
            _dir: dir,
        };
        harness.pump();
        harness
    }

    /// The Jev group: the masked key from `GetJevSettings` is what the
    /// field shows; Save rides `SaveJevSettings` (an untouched field
    /// re-reads the stored key through `RevealJevKey` instead of writing
    /// the mask); the eye reveals and re-conceals; Remove clears the
    /// stored key.
    #[gpui::test]
    fn the_jev_group_masks_saves_reveals_and_removes(cx: &mut gpui::TestAppContext) {
        let harness = jev_harness(
            cx,
            JevSettingsState {
                api_key_masked: Some("sk-j…mnop".into()),
            },
            Some("sk-jev-abcdefghijklmnop"),
        );

        // The masked display, never bullets; configured means no
        // unconfigured note.
        let text = |h: &JevHarness| {
            h.visual
                .read(|cx| h.page.read(cx).jev_key.read(cx).text().to_string())
        };
        let stored = |h: &JevHarness| {
            h.visual.read(|cx| {
                h.page
                    .read(cx)
                    .jev
                    .ready()
                    .and_then(|state| state.api_key_masked.clone())
            })
        };
        assert_eq!(text(&harness), "sk-j…mnop");
        assert_eq!(stored(&harness).as_deref(), Some("sk-j…mnop"));

        // The group is hidden until a Jev feature ships
        // (`JEV_GROUP_VISIBLE`), so drive the page methods its buttons'
        // listeners call.
        assert_eq!(
            harness.visual.debug_bounds("save-jev").is_some(),
            JEV_GROUP_VISIBLE
        );

        // Saving untouched re-reads and re-saves the stored key — the
        // masked display itself is never written as a key.
        harness
            .page
            .update(&mut *harness.visual, |page, cx| page.save_jev(cx));
        harness.pump();
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[serde_json::json!({ "apiKey": "sk-jev-abcdefghijklmnop" })]
        );
        assert_eq!(text(&harness), "sk-j…mnop");

        // The eye reveals through RevealJevKey, then conceals back to the
        // engine's masked display.
        harness
            .page
            .update(&mut *harness.visual, |page, cx| page.toggle_jev_key(cx));
        harness.pump();
        assert_eq!(text(&harness), "sk-jev-abcdefghijklmnop");
        harness
            .page
            .update(&mut *harness.visual, |page, cx| page.toggle_jev_key(cx));
        harness.pump();
        assert_eq!(text(&harness), "sk-j…mnop");

        // Remove returns the group to the unconfigured note.
        harness
            .page
            .update(&mut *harness.visual, |page, cx| page.remove_jev(cx));
        harness.pump();
        assert_eq!(text(&harness), "");
        assert_eq!(stored(&harness), None);
    }
}
