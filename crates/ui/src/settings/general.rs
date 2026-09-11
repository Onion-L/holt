//! The General settings page (automatic-chat-titles ticket 02): everyday
//! preferences that aren't tied to a provider or the agent runtime, starting
//! with the engine-owned title-task settings. The user picks an optional
//! provider-qualified model (empty = automatic titles disabled) and edits the
//! instruction's style notes, both read and saved only through typed RPC —
//! the engine owns `title-settings.json`, validation, and the
//! missing-credentials warning.
//!
//! It also hosts the Web search group (web-tools ticket 07): the user's
//! search backend and its own key, read and saved through the four
//! web-search RPCs. The backend is the user's choice — a same-vendor
//! provider key only ever surfaces as a display-only hint.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, MouseButton, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_proto::{
    Model, Provider, TitleSettingsState, WebSearchBackendOption, WebSearchSettingsState,
};
use holt_rpc::methods;

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

/// The Zhipu vendor's ids in the provider catalog: Z.AI's international and
/// China endpoints. A key stored under either one also works for the Zhipu
/// search backend.
const ZHIPU_PROVIDER_IDS: [&str; 2] = ["zai", "zai-coding-cn"];

/// Whether the Zhipu same-vendor hint shows (spec story 4): the picker is on
/// Zhipu AND one of that vendor's provider keys is configured. Both
/// conditions, exactly — the hint never preselects, prefills, or shares the
/// credential.
fn zhipu_hint_visible(backend: Option<&str>, providers: &[Provider]) -> bool {
    backend == Some("zhipu")
        && providers
            .iter()
            .flat_map(Provider::concrete_providers)
            .any(|provider| {
                provider.configured && ZHIPU_PROVIDER_IDS.contains(&provider.id.as_str())
            })
}

/// One rendered backend row in the Web search picker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendRow {
    id: String,
    name: String,
    /// Settings copy flagging an access requirement (Brave needs
    /// international access).
    note: Option<String>,
    selected: bool,
}

/// The picker's rows: every launch backend the engine offers, in engine
/// order, with the stored selection marked. Unconfigured leaves none marked —
/// the picker starts on no backend, never a default vendor.
fn backend_rows(backends: &[WebSearchBackendOption], selected: Option<&str>) -> Vec<BackendRow> {
    backends
        .iter()
        .map(|backend| BackendRow {
            id: backend.id.clone(),
            name: backend.name.clone(),
            note: backend.note.clone(),
            selected: selected == Some(backend.id.as_str()),
        })
        .collect()
}

/// What the web-search API-key field currently shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyField {
    /// The masked key reported by `GetWebSearchSettings` — untouched.
    Stored,
    /// The raw stored key, fetched on demand by `RevealWebSearchKey`.
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
    /// The engine-owned web-search record (web-tools ticket 07).
    web_search: Loadable<WebSearchSettingsState>,
    /// The provider catalog, read for the Zhipu same-vendor hint.
    providers: Loadable<Vec<Provider>>,
    /// The picker's backend id — the stored one until the user picks again.
    web_search_backend: Option<String>,
    web_search_key: Entity<ComposerInput>,
    /// The raw stored key the field currently shows, fetched by
    /// `RevealWebSearchKey`; `None` while it shows the engine's masked
    /// display or the user's draft.
    web_search_revealed_key: Option<String>,
    /// Draft-only projection: the eye hides the key the user is typing.
    /// Concealed by default, like the provider-key rows.
    web_search_draft_concealed: bool,
    web_search_error: Option<String>,
    backend_menu: Popup<()>,
    /// Re-derives the key field's projection on every edit: a pasted key is
    /// concealed the moment it stops being the masked display.
    _key_events: Subscription,
}

impl GeneralPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let web_search_key = cx.new(|cx| ComposerInput::new_secret("API key", cx));
        let key_events = cx.subscribe(
            &web_search_key,
            |page: &mut Self, _, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    page.sync_key_mask(cx);
                }
            },
        );
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
            web_search: Loadable::Idle,
            providers: Loadable::Idle,
            web_search_backend: None,
            web_search_key,
            web_search_revealed_key: None,
            web_search_draft_concealed: true,
            web_search_error: None,
            backend_menu: Popup::default(),
            _key_events: key_events,
        };
        page.load(cx);
        page
    }

    /// What the key field shows. Derived from the field's text against the
    /// stored masked display rather than tracked through edit events:
    /// programmatic `set_text` emits `Edited` too, so an event-driven flag
    /// cannot tell a draft from a load. A reveal stops being `Revealed` as
    /// soon as the user edits the revealed text.
    fn key_field_state(&self, cx: &App) -> KeyField {
        let text = self.web_search_key.read(cx).text();
        if self.web_search_revealed_key.as_deref() == Some(text) {
            return KeyField::Revealed;
        }
        let stored = self
            .web_search
            .ready()
            .and_then(|state| state.api_key_masked.as_deref());
        if stored == Some(text) {
            KeyField::Stored
        } else {
            KeyField::Draft
        }
    }

    /// Project the field per its state: the engine's masked display and a
    /// revealed key read as plain text (both are already masked, or chosen to
    /// be shown), while a draft renders as bullets unless the eye uncovered
    /// it.
    fn sync_key_mask(&mut self, cx: &mut Context<Self>) {
        let masked = self.key_field_state(cx) == KeyField::Draft && self.web_search_draft_concealed;
        self.web_search_key
            .update(cx, |input, cx| input.set_masked(masked, cx));
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

    /// Echo a web-search reply (read, save, or remove) into the group's
    /// editable state: the picker selection, the key field, and the
    /// reveal/draft flags all reset to the stored truth.
    fn apply_web_search_state(&mut self, state: WebSearchSettingsState, cx: &mut Context<Self>) {
        self.web_search_backend = state.backend.clone();
        let masked = state.api_key_masked.clone().unwrap_or_default();
        // The record lands before the field is re-texted: the edit the
        // `set_text` emits re-derives the projection against this state.
        self.web_search = Loadable::Ready(state);
        self.web_search_revealed_key = None;
        self.web_search_draft_concealed = true;
        self.web_search_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.settings = Loadable::Error("Engine not connected".into());
            self.models = Loadable::Error("Engine not connected".into());
            self.web_search = Loadable::Error("Engine not connected".into());
            return;
        };
        self.settings = Loadable::Loading;
        self.models = Loadable::Loading;
        self.web_search = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let settings_result = engine
                .client()
                .call(methods::GET_TITLE_SETTINGS, serde_json::json!({}))
                .await;
            let (providers, models_result) = match load_providers(&engine).await {
                Ok(providers) => {
                    let models = load_model_catalog(&engine, &providers).await;
                    (Loadable::Ready(providers), models)
                }
                Err(error) => (Loadable::Error(error.clone()), Loadable::Error(error)),
            };
            let web_result = engine
                .client()
                .call(methods::GET_WEB_SEARCH_SETTINGS, serde_json::json!({}))
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
                page.providers = providers;
                page.models = models_result;
                match web_result {
                    Ok(value) => match serde_json::from_value::<WebSearchSettingsState>(value) {
                        Ok(state) => page.apply_web_search_state(state, cx),
                        Err(error) => page.web_search = Loadable::Error(error.to_string()),
                    },
                    // UnknownMethod is version skew, same as the skills page:
                    // name it rather than echoing the raw error.
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        page.web_search = Loadable::Error(
                            "Web search settings aren't available — the engine doesn't support \
                             them yet"
                                .into(),
                        );
                    }
                    Err(error) => page.web_search = Loadable::Error(error.to_string()),
                }
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
        let instruction = effective_instruction(
            self.custom_instruction_enabled,
            self.instruction.read(cx).text(),
        );
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

    /// The eye button on the API-key field. Stored keys are revealed and
    /// concealed through `RevealWebSearchKey`; a draft is only a projection
    /// flip, exactly like the providers page's key rows.
    fn toggle_web_search_key(&mut self, cx: &mut Context<Self>) {
        match self.key_field_state(cx) {
            KeyField::Stored => self.reveal_web_search_key(cx),
            KeyField::Revealed => self.restore_masked_web_search_key(cx),
            KeyField::Draft => {
                self.web_search_draft_concealed = !self.web_search_draft_concealed;
                self.sync_key_mask(cx);
                cx.notify();
            }
        }
    }

    fn reveal_web_search_key(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::REVEAL_WEB_SEARCH_KEY, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match value
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .filter(|key| !key.is_empty())
                    {
                        Some(key) => {
                            // The revealed text is recorded before the field
                            // carries it, so the edit it emits derives
                            // `Revealed` rather than a draft.
                            page.web_search_revealed_key = Some(key.to_string());
                            page.web_search_key.update(cx, |input, cx| {
                                input.set_masked(false, cx);
                                input.set_text(key, cx);
                            });
                            page.web_search_error = None;
                        }
                        None => {
                            page.web_search_error = Some("No API key is stored".into());
                        }
                    },
                    Err(error) => page.web_search_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Conceal again: put the engine's masked key back in the field.
    fn restore_masked_web_search_key(&mut self, cx: &mut Context<Self>) {
        let masked = self
            .web_search
            .ready()
            .and_then(|state| state.api_key_masked.clone())
            .unwrap_or_default();
        self.web_search_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
        self.web_search_revealed_key = None;
        self.web_search_draft_concealed = true;
        cx.notify();
    }

    /// Save the picked backend and the key field's content. An untouched
    /// field holds the engine's masked display, never a usable key: with the
    /// stored backend still picked the stored key is re-read through
    /// `RevealWebSearchKey` and re-saved (a no-op save); picking a different
    /// backend needs its own key rather than silently rebinding another
    /// vendor's.
    fn save_web_search(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let Some(backend) = self.web_search_backend.clone() else {
            self.web_search_error = Some("Choose a search backend".into());
            cx.notify();
            return;
        };
        let stored_backend = self
            .web_search
            .ready()
            .and_then(|state| state.backend.clone());
        let draft = self.web_search_key.read(cx).text().to_string();
        let field_state = self.key_field_state(cx);
        let untouched = field_state == KeyField::Stored;
        // An untouched field or a revealed key both hold the *stored* key,
        // never one entered for the pick: writing it under another backend
        // would silently rebind the wrong vendor's credential.
        if matches!(field_state, KeyField::Stored | KeyField::Revealed)
            && stored_backend.as_deref() != Some(backend.as_str())
        {
            let name = self
                .web_search
                .ready()
                .and_then(|state| {
                    state
                        .backends
                        .iter()
                        .find(|option| option.id == backend)
                        .map(|option| option.name.clone())
                })
                .unwrap_or_else(|| backend.clone());
            self.web_search_error = Some(format!("Enter an API key for {name}"));
            cx.notify();
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let key = if untouched {
                match engine
                    .client()
                    .call(methods::REVEAL_WEB_SEARCH_KEY, serde_json::json!({}))
                    .await
                {
                    Ok(value) => value
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    Err(error) => {
                        this.update(cx, |page, cx| {
                            page.web_search_error = Some(error.to_string());
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
                    page.web_search_error = Some("Enter an API key".into());
                    cx.notify();
                })
                .ok();
                return;
            }
            let result = engine
                .client()
                .call(
                    methods::SAVE_WEB_SEARCH_SETTINGS,
                    serde_json::json!({ "backend": backend, "apiKey": key }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<WebSearchSettingsState>(value) {
                        Ok(state) => {
                            page.apply_web_search_state(state, cx);
                            page.web_search_error = None;
                        }
                        Err(error) => page.web_search_error = Some(error.to_string()),
                    },
                    Err(error) => page.web_search_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Clear the record — the unconfigured state is back to "no tool".
    fn remove_web_search(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::REMOVE_WEB_SEARCH_SETTINGS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        let backends = page
                            .web_search
                            .ready()
                            .map(|state| state.backends.clone())
                            .unwrap_or_default();
                        page.web_search_error = None;
                        page.apply_web_search_state(
                            WebSearchSettingsState {
                                backend: None,
                                api_key_masked: None,
                                backends,
                            },
                            cx,
                        );
                    }
                    Err(error) => page.web_search_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The device-local Notifications group (issue 03). Independent of the
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
            .mt(px(28.0))
            .child(widgets::section_label(theme, "Notifications"))
            .child(div().mt(px(4.0)).child(widgets::row_description(
                theme,
                "Device-local. Banners appear only while no Holt window is active.",
            )))
            .child(
                widgets::flat_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(widgets::row_title(theme, "Completion notifications"))
                            .child(widgets::row_description(
                                theme,
                                "Show a system banner when a background Turn succeeds or fails.",
                            )),
                    )
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
                widgets::flat_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(widgets::row_title(theme, "Play sound"))
                            .child(widgets::row_description(
                                theme,
                                "Play the system notification sound with each banner.",
                            )),
                    )
                    .child(sound_switch),
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

    fn close_backend_menu(&mut self, cx: &mut Context<Self>) {
        if self.backend_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.backend_menu);
        }
    }

    fn toggle_backend_menu(&mut self, cx: &mut Context<Self>) {
        if self.backend_menu.take_press_was_open() || self.backend_menu.is_open() {
            self.close_backend_menu(cx);
        } else {
            self.backend_menu.open(());
        }
        cx.notify();
    }

    /// The Web search group (web-tools ticket 07): the backend picker, the
    /// key field with the provider-key rows' reveal affordance, and the
    /// Save/Remove actions. The group's state is independent of the
    /// title-settings group above: a title-settings failure never hides it.
    fn render_web_search(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.web_search {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("web-search-skeleton", theme, 2, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => widgets::error_strip(theme, error.clone())
                .id("web-search-unavailable")
                .debug_selector(|| "web-search-unavailable".into())
                .into_any_element(),
            Loadable::Ready(state) => {
                let unconfigured = state.backend.is_none();
                let hint = zhipu_hint_visible(
                    self.web_search_backend.as_deref(),
                    self.providers.ready().map(Vec::as_slice).unwrap_or(&[]),
                );
                let rows = backend_rows(&state.backends, self.web_search_backend.as_deref());
                // No stored pick leaves the trigger on explicit copy: the
                // backend is the user's choice, never an app preselection.
                let selected_label = rows
                    .iter()
                    .find(|row| row.selected)
                    .map(|row| row.name.clone())
                    .unwrap_or_else(|| "Select a backend".to_string());

                let menu_rows = rows.iter().enumerate().map(|(index, row)| {
                    let id = row.id.clone();
                    let selected = row.selected;
                    let name = row.name.clone();
                    let note = row.note.clone();
                    popover::menu_row(
                        theme,
                        selected,
                        format!("web-search-backend-option-{index}"),
                    )
                    .id(SharedString::from(format!(
                        "web-search-backend-option-{index}"
                    )))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.web_search_backend = Some(id.clone());
                        page.close_backend_menu(cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().truncate().child(SharedString::from(name)))
                            .children(note.map(|note| {
                                div()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(
                                        widgets::ROW_DESCRIPTION_SIZE,
                                    ))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(note))
                            })),
                    )
                    .when(selected, |row| {
                        row.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(14.0))
                                .text_color(theme.accent),
                        )
                    })
                    .into_any_element()
                });
                let backend_menu = popover::popover_card(theme)
                    .id("web-search-backend-scroll")
                    .w(px(320.0))
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_backend_menu(cx)))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(menu_rows)
                    .into_any_element();
                let backend_trigger = div()
                    .id("web-search-backend-dropdown")
                    .debug_selector(|| "web-search-backend-dropdown".into())
                    .relative()
                    .w(px(220.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if self.backend_menu.is_open() {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .bg(theme.input_glass_bg())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|page, _, _, _| page.backend_menu.note_trigger_press()),
                    )
                    .on_click(cx.listener(|page, _, _, cx| page.toggle_backend_menu(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(selected_label)),
                    )
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .when_some(self.backend_menu.get(), |trigger, _| {
                        trigger.child(popover::anchored_menu_below(
                            "web-search-backend-menu",
                            backend_menu,
                            self.backend_menu.closing_since(),
                        ))
                    });

                let mut column = div().flex().flex_col();
                if unconfigured {
                    column = column.child(
                        div()
                            .id("web-search-unconfigured")
                            .debug_selector(|| "web-search-unconfigured".into())
                            .pb(px(4.0))
                            .child(widgets::row_description(
                                theme,
                                "No backend configured — the agent has no web search tool.",
                            )),
                    );
                }
                column = column.child(
                    widgets::flat_row()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(widgets::row_title(theme, "Backend"))
                                .child(widgets::row_description(
                                    theme,
                                    "The search service the agent queries.",
                                )),
                        )
                        .child(backend_trigger),
                );
                column = column.child(
                    widgets::flat_row()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(widgets::row_title(theme, "API key"))
                                .child(widgets::row_description(
                                    theme,
                                    "Stored on this device, separate from your provider keys.",
                                )),
                        )
                        .child(web_search_key_field(
                            theme,
                            self.web_search_key.clone(),
                            self.key_field_state(cx),
                            self.web_search_draft_concealed,
                            cx,
                        )),
                );
                if hint {
                    column = column.child(
                        div()
                            .id("web-search-hint")
                            .debug_selector(|| "web-search-hint".into())
                            .mt(px(-6.0))
                            .child(widgets::row_description(
                                theme,
                                "An existing Zhipu provider key is configured; it works for \
                                 Zhipu search too.",
                            )),
                    );
                }

                let save_theme = theme.clone();
                let save = widgets::ghost_action(theme)
                    .id("save-web-search")
                    .debug_selector(|| "save-web-search".into())
                    .border_1()
                    .border_color(theme.border)
                    .hover(move |style| widgets::ghost_hover(&save_theme, style))
                    .on_click(cx.listener(|page, _, _, cx| page.save_web_search(cx)))
                    .child("Save");
                let danger = theme.danger;
                let danger_muted = theme.danger_muted;
                let remove = widgets::ghost_action(theme)
                    .id("remove-web-search")
                    .debug_selector(|| "remove-web-search".into())
                    .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                    .on_click(cx.listener(|page, _, _, cx| page.remove_web_search(cx)))
                    .child("Remove");
                column = column.child(
                    div()
                        .mt(px(8.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(remove)
                        .child(save),
                );
                if let Some(error) = self.web_search_error.clone() {
                    column = column.child(
                        widgets::error_strip(theme, error)
                            .id("web-search-error")
                            .debug_selector(|| "web-search-error".into()),
                    );
                }
                column.into_any_element()
            }
        };
        div()
            .id("web-search-group")
            .mt(px(32.0))
            .child(widgets::section_label(theme, "Web search"))
            .child(div().mt(px(4.0)).child(widgets::row_description(
                theme,
                "One search service, chosen by you, with its own key. Without one the agent has \
                 no web search tool; reading a page is a separate tool.",
            )))
            .child(div().mt(px(12.0)).child(body))
            .into_any_element()
    }
}

/// The API-key field for the Web search group: the bordered input carrying
/// the engine's masked key (or the user's draft) with the in-field eye
/// toggle — the same affordance the provider-key rows use.
fn web_search_key_field(
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
        .id("web-search-key-field")
        .debug_selector(|| "web-search-key-field".into())
        .w(px(300.0))
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
                .id("toggle-web-search-key")
                .debug_selector(|| "toggle-web-search-key".into())
                .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                .on_click(cx.listener(|page, _, _, cx| page.toggle_web_search_key(cx)))
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
}

/// The provider catalog behind the Zhipu same-vendor hint.
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
                            page.selected_model = row_id.clone();
                            page.close_model_menu(cx);
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
                    .w(px(220.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if self.model_menu.is_open() {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .bg(theme.input_glass_bg())
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

                let save_theme = theme.clone();
                let mut column = div().flex().flex_col();
                column = column.child(
                    widgets::flat_row()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(widgets::row_title(&theme, "Title model"))
                                .child(widgets::row_description(
                                    &theme,
                                    "Choose a configured provider model. Disabled keeps the fallback title.",
                                )),
                        )
                        .child(model_trigger),
                );
                if let Some(warning) = state.warning.clone() {
                    column = column.child(widgets::warning_strip(&theme, warning));
                }
                column = column.child(
                    widgets::flat_row()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(widgets::row_title(&theme, "Custom title style"))
                                .child(widgets::row_description(
                                    &theme,
                                    "Style notes for automatic titles — language, tone, naming \
                                     conventions. The core naming rules are built in.",
                                )),
                        )
                        .child(
                            div()
                                .id("custom-title-prompt-toggle")
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
                    column = column.child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(widgets::field_label(&theme, "Title style notes"))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("restore-title-instruction")
                                    .hover(move |style| widgets::ghost_hover(&restore_theme, style))
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
                }
                column = column.child(
                    div().mt(px(8.0)).flex().flex_row().justify_end().child(
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
            .id("general-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "General", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Everyday preferences — starting with how new chats are named.",
                    ))
                    .child(Self::render_notifications(&theme, cx))
                    .child(
                        div()
                            .mt(px(32.0))
                            .child(widgets::section_label(&theme, "Automatic chat titles")),
                    )
                    .child(div().mt(px(4.0)).child(widgets::row_description(
                        &theme,
                        "A new chat keeps its first-line title immediately, then one \
                             background request to this model can replace it. Your manual \
                             renames always win.",
                    )))
                    .child(div().mt(px(12.0)).child(body))
                    .child(self.render_web_search(&theme, cx)),
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

    // ---- Web search group (web-tools ticket 07) ----

    fn backend_option(id: &str, name: &str, note: Option<&str>) -> WebSearchBackendOption {
        WebSearchBackendOption {
            id: id.into(),
            name: name.into(),
            note: note.map(str::to_string),
        }
    }

    fn launch_backends() -> Vec<WebSearchBackendOption> {
        vec![
            backend_option("zhipu", "Zhipu", None),
            backend_option("bocha", "Bocha", None),
            backend_option("brave", "Brave", Some("Needs international access")),
        ]
    }

    /// A provider row whose single variant carries the same id — the
    /// concrete shape `LIST_PROVIDERS` flattens to.
    fn provider(id: &str, configured: bool) -> Provider {
        Provider {
            id: ProviderId::from(id),
            name: id.into(),
            abbreviation: "X".into(),
            configured,
            variants: vec![ProviderVariant {
                id: ProviderId::from(id),
                name: id.into(),
                configured,
            }],
        }
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

    #[test]
    fn backend_rows_mark_only_the_stored_selection() {
        let rows = backend_rows(&launch_backends(), Some("bocha"));
        assert_eq!(rows.len(), 3);
        assert!(rows[1].selected);
        assert_eq!(rows[1].note, None);
        assert_eq!(rows[2].note.as_deref(), Some("Needs international access"));
        // Unconfigured: no vendor starts selected.
        assert!(
            backend_rows(&launch_backends(), None)
                .iter()
                .all(|row| !row.selected)
        );
    }

    #[test]
    fn the_zhipu_hint_needs_both_the_pick_and_a_zhipu_provider_key() {
        let zhipu_configured = [provider("zai", true)];
        assert!(zhipu_hint_visible(Some("zhipu"), &zhipu_configured));
        // Zhipu's China endpoint is the same vendor.
        assert!(zhipu_hint_visible(
            Some("zhipu"),
            &[provider("zai-coding-cn", true)]
        ));
        // Another backend picked: no hint, whatever key exists.
        assert!(!zhipu_hint_visible(Some("bocha"), &zhipu_configured));
        // No pick yet: no hint.
        assert!(!zhipu_hint_visible(None, &zhipu_configured));
        // A Zhipu provider without a key: no hint.
        assert!(!zhipu_hint_visible(
            Some("zhipu"),
            &[provider("zai", false)]
        ));
        // A configured key from another vendor: no hint.
        assert!(!zhipu_hint_visible(
            Some("zhipu"),
            &[provider("anthropic", true)]
        ));
    }

    /// The Web search group's engine seam: the four web-search methods plus
    /// the provider/title reads the page performs on load. Everything else is
    /// version skew and parks the AppState's standing watches on the retry
    /// timer (the notifications harness' stance).
    struct FakeWebSearchEngine {
        state: std::sync::Mutex<WebSearchSettingsState>,
        key: std::sync::Mutex<Option<String>>,
        providers: std::sync::Mutex<Vec<Provider>>,
        saved: std::sync::Mutex<Vec<serde_json::Value>>,
        /// False stands in for an engine that predates the web-search RPCs.
        available: bool,
    }

    #[async_trait::async_trait]
    impl holt_rpc::RpcService for FakeWebSearchEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
            use holt_rpc::{RpcError, RpcReply};
            if !self.available
                && matches!(
                    method,
                    methods::GET_WEB_SEARCH_SETTINGS
                        | methods::SAVE_WEB_SEARCH_SETTINGS
                        | methods::REVEAL_WEB_SEARCH_KEY
                        | methods::REMOVE_WEB_SEARCH_SETTINGS
                )
            {
                return Err(RpcError::UnknownMethod(method.to_string()));
            }
            match method {
                methods::GET_TITLE_SETTINGS => RpcReply::value(&serde_json::json!({
                    "settings": {
                        "modelId": null,
                        "instruction": holt_proto::DEFAULT_TITLE_INSTRUCTION,
                    },
                })),
                methods::LIST_PROVIDERS => RpcReply::value(&*self.providers.lock().unwrap()),
                methods::LIST_MODELS => RpcReply::value(&serde_json::json!([])),
                methods::GET_WEB_SEARCH_SETTINGS => RpcReply::value(&*self.state.lock().unwrap()),
                methods::REVEAL_WEB_SEARCH_KEY => RpcReply::value(&serde_json::json!({
                    "key": self.key.lock().unwrap().clone(),
                })),
                methods::SAVE_WEB_SEARCH_SETTINGS => {
                    let backend = params
                        .get("backend")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let api_key = params
                        .get("apiKey")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let known = self
                        .state
                        .lock()
                        .unwrap()
                        .backends
                        .iter()
                        .any(|option| option.id == backend);
                    if !known {
                        return Err(RpcError::BadParams(format!(
                            "unknown search backend {backend:?}"
                        )));
                    }
                    if api_key.trim().is_empty() {
                        return Err(RpcError::BadParams("apiKey is required".into()));
                    }
                    self.saved.lock().unwrap().push(params);
                    let backends = self.state.lock().unwrap().backends.clone();
                    *self.key.lock().unwrap() = Some(api_key.clone());
                    *self.state.lock().unwrap() = WebSearchSettingsState {
                        backend: Some(backend),
                        api_key_masked: Some(masked(&api_key)),
                        backends,
                    };
                    RpcReply::value(&*self.state.lock().unwrap())
                }
                methods::REMOVE_WEB_SEARCH_SETTINGS => {
                    let backends = self.state.lock().unwrap().backends.clone();
                    *self.key.lock().unwrap() = None;
                    *self.state.lock().unwrap() = WebSearchSettingsState {
                        backend: None,
                        api_key_masked: None,
                        backends,
                    };
                    RpcReply::value(&serde_json::json!({}))
                }
                _ => Err(RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    struct WebSearchHarness<'a> {
        page: Entity<GeneralPage>,
        visual: &'a mut gpui::VisualTestContext,
        engine: std::sync::Arc<FakeWebSearchEngine>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl WebSearchHarness<'_> {
        /// Drive RPC dispatch a few rounds (test thread), then the gpui
        /// foreground executor — the notifications harness' pump.
        fn pump(&self) {
            for _ in 0..6 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                self.visual.run_until_parked();
            }
        }

        /// The key field's committed text — the masked key from Get until the
        /// user drafts or reveals.
        fn key_text(&self) -> String {
            self.visual.read(|cx| {
                self.page
                    .read(cx)
                    .web_search_key
                    .read(cx)
                    .text()
                    .to_string()
            })
        }

        /// Whether the field renders its content as bullets.
        fn key_masked(&self) -> bool {
            self.visual
                .read(|cx| self.page.read(cx).web_search_key.read(cx).is_masked())
        }

        fn draft(&mut self, backend: &str, key: &str) {
            self.page.update(&mut *self.visual, |page, cx| {
                page.web_search_backend = Some(backend.to_string());
                page.web_search_key.update(cx, |input, cx| {
                    input.set_text(key, cx);
                });
                cx.notify();
            });
            self.pump();
        }

        fn click(&mut self, selector: &'static str) {
            let bounds = self
                .visual
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} renders"));
            self.visual
                .simulate_click(bounds.center(), Default::default());
            self.pump();
        }
    }

    fn web_search_harness<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: WebSearchSettingsState,
        key: Option<&str>,
        providers: Vec<Provider>,
    ) -> WebSearchHarness<'a> {
        web_search_harness_with(cx, state, key, providers, true)
    }

    fn web_search_harness_with<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: WebSearchSettingsState,
        key: Option<&str>,
        providers: Vec<Provider>,
        available: bool,
    ) -> WebSearchHarness<'a> {
        let engine = std::sync::Arc::new(FakeWebSearchEngine {
            state: std::sync::Mutex::new(state),
            key: std::sync::Mutex::new(key.map(str::to_string)),
            providers: std::sync::Mutex::new(providers),
            saved: std::sync::Mutex::new(Vec::new()),
            available,
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
        let harness = WebSearchHarness {
            page,
            visual,
            engine,
            runtime,
            _dir: dir,
        };
        harness.pump();
        harness
    }

    /// The configured path: the masked key from `GetWebSearchSettings` is what
    /// the field shows, the same-vendor hint needs both conditions, and Save
    /// rides `SaveWebSearchSettings`.
    #[gpui::test]
    fn the_group_shows_the_masked_key_and_saves_through_the_rpc(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(
            cx,
            WebSearchSettingsState {
                backend: Some("zhipu".into()),
                api_key_masked: Some("sk-1…cdef".into()),
                backends: launch_backends(),
            },
            Some("sk-1234567890abcdef"),
            vec![provider("zai", true)],
        );
        assert_eq!(harness.key_text(), "sk-1…cdef");
        // The engine's masked display reads as plain text — it is already
        // masked; bullets would hide the first/last characters it exists to
        // show.
        assert!(!harness.key_masked());
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unconfigured")
                .is_none()
        );
        assert!(harness.visual.debug_bounds("web-search-hint").is_some());

        // Saving untouched re-reads and re-saves the stored key — the masked
        // display itself is never written as a key.
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[serde_json::json!({
                "backend": "zhipu",
                "apiKey": "sk-1234567890abcdef",
            })]
        );

        // The eye reveals the stored key through RevealWebSearchKey, then
        // conceals it back to the engine's masked display.
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), "sk-1234567890abcdef");
        assert!(!harness.key_masked());
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), "sk-1…cdef");
        assert!(!harness.key_masked());

        // A revealed key is still the *stored* key: picking another backend
        // and saving must not rebind it to that backend.
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), "sk-1234567890abcdef");
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.web_search_backend = Some("brave".into());
            cx.notify();
        });
        harness.pump();
        assert!(harness.visual.debug_bounds("web-search-hint").is_none());
        harness.click("save-web-search");
        assert_eq!(harness.engine.saved.lock().unwrap().len(), 1);
        let error = harness
            .visual
            .read(|cx| harness.page.read(cx).web_search_error.clone());
        assert_eq!(error.as_deref(), Some("Enter an API key for Brave"));

        // Editing a revealed key makes it a draft: the eye is then only a
        // projection flip, never a restore that discards what was typed.
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.web_search_backend = Some("zhipu".into());
            page.web_search_key
                .update(cx, |input, cx| input.set_text("typed-after-reveal", cx));
            cx.notify();
        });
        harness.pump();
        assert!(harness.key_masked(), "a draft is concealed by default");
        harness.click("toggle-web-search-key");
        assert!(!harness.key_masked());
        assert_eq!(harness.key_text(), "typed-after-reveal");
        harness.click("toggle-web-search-key");
        assert!(harness.key_masked());
        assert_eq!(harness.key_text(), "typed-after-reveal");

        // A draft key saves with the picked backend.
        harness.draft("bocha", "bocha-key-0000");
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[
                serde_json::json!({ "backend": "zhipu", "apiKey": "sk-1234567890abcdef" }),
                serde_json::json!({ "backend": "bocha", "apiKey": "bocha-key-0000" }),
            ]
        );
        // The save reply re-echoes the masked key and the draft is gone.
        assert_eq!(harness.key_text(), masked("bocha-key-0000"));
        assert!(harness.visual.debug_bounds("web-search-error").is_none());

        // Remove clears the record: the group reads unconfigured again.
        harness.click("remove-web-search");
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unconfigured")
                .is_some()
        );
        assert_eq!(harness.key_text(), "");
    }

    /// Layout invariant: the in-field eye toggle stays inside the key field's
    /// box. The input's `w_full` root in a fixed-width flex row pushed the
    /// `flex_none` toggle out past the field's right border (user report).
    #[gpui::test]
    fn the_eye_toggle_stays_inside_the_key_field(cx: &mut gpui::TestAppContext) {
        let harness = web_search_harness(
            cx,
            WebSearchSettingsState {
                backend: Some("zhipu".into()),
                api_key_masked: Some("sk-1…cdef".into()),
                backends: launch_backends(),
            },
            Some("sk-1234567890abcdef"),
            vec![],
        );
        let field = harness
            .visual
            .debug_bounds("web-search-key-field")
            .expect("key field renders");
        let eye = harness
            .visual
            .debug_bounds("toggle-web-search-key")
            .expect("eye toggle renders");
        assert!(
            eye.right() <= field.right(),
            "eye toggle {eye:?} escapes the key field {field:?}"
        );
    }

    /// Unconfigured is not an error: the group explains the missing tool, the
    /// field starts empty, and no vendor is preselected — not even when a
    /// Zhipu provider key exists.
    #[gpui::test]
    fn an_unconfigured_backend_reads_as_no_tool_not_an_error(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(
            cx,
            WebSearchSettingsState {
                backend: None,
                api_key_masked: None,
                backends: launch_backends(),
            },
            None,
            vec![provider("zai", true)],
        );
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unconfigured")
                .is_some()
        );
        assert!(harness.visual.debug_bounds("web-search-error").is_none());
        assert!(harness.visual.debug_bounds("web-search-hint").is_none());
        assert_eq!(harness.key_text(), "");

        // An untouched field carries no key: Save refuses locally instead of
        // writing the masked display.
        harness.click("save-web-search");
        assert!(harness.engine.saved.lock().unwrap().is_empty());
        let error = harness
            .visual
            .read(|cx| harness.page.read(cx).web_search_error.clone());
        assert_eq!(error.as_deref(), Some("Choose a search backend"));

        // Picking a backend and pasting its key configures the group.
        harness.draft("zhipu", "zhipu-key-123456");
        assert!(
            harness.key_masked(),
            "a pasted key is concealed the moment it stops being the masked display"
        );
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[serde_json::json!({ "backend": "zhipu", "apiKey": "zhipu-key-123456" })]
        );
        assert_eq!(harness.key_text(), masked("zhipu-key-123456"));
        assert!(!harness.key_masked());
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unconfigured")
                .is_none()
        );
        // The pick now satisfies the hint's first condition.
        assert!(harness.visual.debug_bounds("web-search-hint").is_some());
    }

    /// A rejected save surfaces inline, never as a window-top modal, and the
    /// group keeps its state.
    #[gpui::test]
    fn a_rejected_save_surfaces_inline(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(
            cx,
            WebSearchSettingsState {
                backend: Some("zhipu".into()),
                api_key_masked: Some("sk-1…cdef".into()),
                backends: launch_backends(),
            },
            Some("sk-1234567890abcdef"),
            vec![],
        );
        // Force an id the engine does not offer — the save RPC's validation.
        harness.draft("bogus", "some-key-000000");
        harness.click("save-web-search");
        assert!(harness.engine.saved.lock().unwrap().is_empty());
        let error = harness
            .visual
            .read(|cx| harness.page.read(cx).web_search_error.clone());
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("unknown search backend")),
            "unexpected error: {error:?}"
        );
        assert!(harness.visual.debug_bounds("web-search-error").is_some());
        // The rejected draft stays in the field; nothing was written.
        assert_eq!(harness.key_text(), "some-key-000000");
    }

    /// An engine without the web-search RPCs reads as version skew — a named
    /// message, not a raw error and not a silent empty group.
    #[gpui::test]
    fn an_engine_without_the_web_search_rpcs_names_the_skew(cx: &mut gpui::TestAppContext) {
        let harness = web_search_harness_with(
            cx,
            WebSearchSettingsState {
                backend: None,
                api_key_masked: None,
                backends: launch_backends(),
            },
            None,
            vec![],
            false,
        );
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unavailable")
                .is_some()
        );
        let error = harness
            .visual
            .read(|cx| harness.page.read(cx).web_search.clone());
        assert!(
            matches!(error, Loadable::Error(ref message) if message.contains("aren't available")),
            "unexpected state: {error:?}"
        );
        assert!(
            harness
                .visual
                .debug_bounds("web-search-backend-dropdown")
                .is_none()
        );
    }
}
