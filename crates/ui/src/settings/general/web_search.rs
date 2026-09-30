//! The Web search group on the General page (web-tools ticket 07, ADR-0023):
//! one service dropdown — Off, then the built-in backends in engine order
//! (keyless Exa first). A keyless or already-configured service switches on
//! pick; a keyed one without a key asks for it first, and saving the key
//! activates it. The active keyed service shows its key field below.
//!
//! Search keys are the user's own records: never prefilled from, or shared
//! with, a same-vendor provider key.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, MouseButton, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_proto::{WebSearchBackendOption, WebSearchEntryView, WebSearchSettingsState};
use holt_rpc::methods;

use super::{
    CONTROL_HEIGHT, CONTROL_WIDTH, GROUP_GAP, KeyField, control_with_actions, group_header,
    group_row, group_rows, remove_button, row_text, save_button,
};
use crate::{
    composer::{ComposerInput, ComposerInputEvent},
    popover::{self, Loadable, Popup},
    settings::widgets,
    state::AppState,
    theme::Theme,
};

pub struct WebSearchGroup {
    state: Entity<AppState>,
    web_search: Loadable<WebSearchSettingsState>,
    menu: Popup<()>,
    /// A keyed service picked before it has a key: the field asks for one,
    /// and nothing switches until it is saved.
    pending: Option<String>,
    key: Entity<ComposerInput>,
    /// The raw stored key the field currently shows, fetched by
    /// `RevealWebSearchKey`; `None` while it shows the engine's masked
    /// display or the user's draft.
    revealed_key: Option<String>,
    /// Draft-only projection: the eye hides the key the user is typing.
    draft_concealed: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    /// Re-derives the key field's projection on every edit: a pasted key is
    /// concealed the moment it stops being the masked display.
    _key_events: Subscription,
}

impl WebSearchGroup {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let key = cx.new(|cx| ComposerInput::new_secret("API key", cx));
        let key_events = cx.subscribe(&key, |group: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                group.sync_key_mask(cx);
            }
        });
        let mut group = Self {
            state,
            web_search: Loadable::Idle,
            menu: Popup::default(),
            pending: None,
            key,
            revealed_key: None,
            draft_concealed: true,
            error: None,
            task: None,
            _key_events: key_events,
        };
        group.load(cx);
        group
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search = Loadable::Error("Engine not connected".into());
            return;
        };
        self.web_search = Loadable::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_WEB_SEARCH_SETTINGS, serde_json::json!({}))
                .await;
            this.update(cx, |group, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<WebSearchSettingsState>(value) {
                        Ok(state) => group.apply_state(state, cx),
                        Err(error) => group.web_search = Loadable::Error(error.to_string()),
                    },
                    // UnknownMethod is version skew, same as the skills page:
                    // name it rather than echoing the raw error.
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        group.web_search = Loadable::Error(
                            "Web search settings aren't available — the engine doesn't support \
                             them yet"
                                .into(),
                        );
                    }
                    Err(error) => group.web_search = Loadable::Error(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn entry(&self, id: &str) -> Option<&WebSearchEntryView> {
        self.web_search
            .ready()?
            .entries
            .iter()
            .find(|entry| entry.id == id)
    }

    fn backend(&self, id: &str) -> Option<&WebSearchBackendOption> {
        self.web_search
            .ready()?
            .backends
            .iter()
            .find(|backend| backend.id == id)
    }

    /// The service the dropdown shows: a pending pick, else the active one.
    fn shown(&self) -> Option<String> {
        self.pending
            .clone()
            .or_else(|| self.web_search.ready()?.active.clone())
    }

    /// The kind the key field belongs to: the shown service, when it takes
    /// a key.
    fn key_kind(&self) -> Option<String> {
        self.shown().filter(|id| {
            self.pending.as_deref() == Some(id.as_str())
                || self.backend(id).is_some_and(|backend| backend.needs_key)
        })
    }

    fn stored_masked_key(&self) -> Option<String> {
        self.entry(&self.key_kind()?)?.api_key_masked.clone()
    }

    /// What the key field currently shows. Derived from the field's text
    /// against the stored masked display rather than tracked through edit
    /// events: programmatic `set_text` emits `Edited` too.
    fn key_field_state(&self, cx: &App) -> KeyField {
        let text = self.key.read(cx).text();
        if self.revealed_key.as_deref() == Some(text) {
            return KeyField::Revealed;
        }
        if self.stored_masked_key().as_deref() == Some(text) {
            KeyField::Stored
        } else {
            KeyField::Draft
        }
    }

    /// The engine's masked display and a revealed key read as plain text; a
    /// draft renders as bullets unless the eye uncovered it.
    fn sync_key_mask(&mut self, cx: &mut Context<Self>) {
        let masked = self.key_field_state(cx) == KeyField::Draft && self.draft_concealed;
        self.key
            .update(cx, |input, cx| input.set_masked(masked, cx));
    }

    /// Refill the key field with the stored masked key of the current
    /// [`Self::key_kind`] (empty when there is none).
    fn reset_key_field(&mut self, cx: &mut Context<Self>) {
        let text = self.stored_masked_key().unwrap_or_default();
        self.revealed_key = None;
        self.draft_concealed = true;
        self.key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(text, cx);
        });
    }

    /// A dropdown pick; `None` is Off.
    fn pick(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.error = None;
        let Some(id) = id else {
            self.pending = None;
            self.call(
                methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
                serde_json::json!({ "id": null }),
                cx,
            );
            return;
        };
        let Some(needs_key) = self.backend(&id).map(|backend| backend.needs_key) else {
            return;
        };
        let active = self
            .web_search
            .ready()
            .and_then(|state| state.active.clone());
        if self.entry(&id).is_some() {
            self.pending = None;
            if active.as_deref() == Some(id.as_str()) {
                self.reset_key_field(cx);
                cx.notify();
            } else {
                self.call(
                    methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
                    serde_json::json!({ "id": id }),
                    cx,
                );
            }
        } else if needs_key {
            // Set before the field refills, so its edit derives against
            // the right kind.
            self.pending = Some(id);
            self.reset_key_field(cx);
            cx.notify();
        } else {
            self.pending = None;
            self.call(
                methods::SAVE_WEB_SEARCH_BACKEND,
                serde_json::json!({ "kind": id }),
                cx,
            );
        }
    }

    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.error = None;
        self.reset_key_field(cx);
        cx.notify();
    }

    /// Send a web-search RPC that replies the settings state.
    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_error("Engine not connected".into(), cx);
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WebSearchSettingsState>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |group, cx| match result {
                Ok(state) => group.apply_state(state, cx),
                Err(error) => group.set_error(error, cx),
            })
            .ok();
        }));
    }

    fn apply_state(&mut self, state: WebSearchSettingsState, cx: &mut Context<Self>) {
        self.web_search = Loadable::Ready(state);
        self.error = None;
        self.pending = None;
        self.reset_key_field(cx);
        cx.notify();
    }

    fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    /// The eye on the key field: a stored key is revealed and concealed
    /// through `RevealWebSearchKey`; a draft only flips its projection.
    fn toggle_key(&mut self, cx: &mut Context<Self>) {
        match self.key_field_state(cx) {
            KeyField::Stored => self.reveal_key(cx),
            KeyField::Revealed => {
                self.reset_key_field(cx);
                cx.notify();
            }
            KeyField::Draft => {
                self.draft_concealed = !self.draft_concealed;
                self.sync_key_mask(cx);
                cx.notify();
            }
        }
    }

    fn reveal_key(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_error("Engine not connected".into(), cx);
            return;
        };
        let Some(id) = self.key_kind() else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REVEAL_WEB_SEARCH_KEY,
                    serde_json::json!({ "id": id.clone() }),
                )
                .await;
            this.update(cx, |group, cx| {
                // The field moved to another service meanwhile: the key is
                // not its to show.
                if group.key_kind().as_deref() != Some(id.as_str()) {
                    return;
                }
                match result {
                    Ok(value) => match value
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .filter(|key| !key.is_empty())
                    {
                        Some(key) => {
                            // Recorded before the field carries it, so the
                            // edit it emits derives `Revealed`, not a draft.
                            group.revealed_key = Some(key.to_string());
                            group.key.update(cx, |input, cx| {
                                input.set_masked(false, cx);
                                input.set_text(key, cx);
                            });
                            group.error = None;
                        }
                        None => group.error = Some("No API key is stored".into()),
                    },
                    Err(error) => group.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Save the field's key (which also activates the service). An
    /// untouched field writes nothing — the masked display is never a key.
    fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.key_kind() else {
            return;
        };
        if self.key_field_state(cx) != KeyField::Draft {
            self.reset_key_field(cx);
            cx.notify();
            return;
        }
        let key = self.key.read(cx).text().trim().to_string();
        if key.is_empty() {
            self.set_error("Enter an API key".into(), cx);
            return;
        }
        self.call(
            methods::SAVE_WEB_SEARCH_BACKEND,
            serde_json::json!({ "kind": kind, "apiKey": key }),
            cx,
        );
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.key_kind() else {
            return;
        };
        self.call(
            methods::REMOVE_WEB_SEARCH_BACKEND,
            serde_json::json!({ "id": id }),
            cx,
        );
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.begin_close() {
            popover::reap_popup(cx, |group| &mut group.menu);
        }
    }

    fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take_press_was_open() || self.menu.is_open() {
            self.close_menu(cx);
        } else {
            self.menu.open(());
        }
        cx.notify();
    }

    fn render_menu(
        &self,
        theme: &Theme,
        state: &WebSearchSettingsState,
        shown: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let off = (
            None,
            "Off".to_string(),
            "The agent can't search the web".to_string(),
        );
        let options = state.backends.iter().map(|backend| {
            let detail = if !backend.needs_key {
                "No API key needed"
            } else if self.entry(&backend.id).is_some() {
                "API key saved"
            } else {
                "Uses your API key"
            };
            (
                Some(backend.id.clone()),
                backend.name.clone(),
                detail.to_string(),
            )
        });
        let rows = std::iter::once(off)
            .chain(options)
            .map(|(id, title, detail)| {
                let selected = id.as_deref() == shown;
                let row_id = format!("web-search-option-{}", id.as_deref().unwrap_or("off"));
                let selector = row_id.clone();
                popover::menu_row(theme, selected, row_id.clone())
                    .id(SharedString::from(row_id))
                    .debug_selector(move || selector.clone())
                    .on_click(cx.listener(move |group, _, _, cx| {
                        cx.stop_propagation();
                        group.close_menu(cx);
                        group.pick(id.clone(), cx);
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
                    .when(selected, |row| {
                        row.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(14.0))
                                .text_color(theme.accent),
                        )
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        popover::popover_card(theme)
            .id("web-search-menu-card")
            .w(px(CONTROL_WIDTH))
            .on_mouse_down_out(cx.listener(|group, _, _, cx| group.close_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows)
            .into_any_element()
    }

    fn render_trigger(
        &self,
        theme: &Theme,
        label: String,
        menu: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("web-search-service")
            .debug_selector(|| "web-search-service".into())
            .relative()
            .flex_none()
            .w(px(CONTROL_WIDTH))
            .h(px(CONTROL_HEIGHT))
            .px(px(10.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(if self.menu.is_open() {
                theme.ink(0.09)
            } else {
                theme.ink(0.05)
            })
            .when(!self.menu.is_open(), |el| {
                el.hover(|style| style.bg(crate::theme::ink(0.07)))
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|group, _, _, _| group.menu.note_trigger_press()),
            )
            .on_click(cx.listener(|group, _, _, cx| group.toggle_menu(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(label)),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.menu.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "web-search-menu",
                    menu,
                    self.menu.closing_since(),
                ))
            })
            .into_any_element()
    }

    fn render_key_field(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let field = self.key_field_state(cx);
        let showing_plain =
            field == KeyField::Revealed || (field == KeyField::Draft && !self.draft_concealed);
        let hover_theme = theme.clone();
        div()
            .id("web-search-key-field")
            .debug_selector(|| "web-search-key-field".into())
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
            // The input's root is `w_full`: without a shrinkable track it
            // claims the whole box and pushes the flex-none eye out.
            .child(div().flex_1().min_w_0().child(self.key.clone()))
            .child(
                widgets::ghost_action(theme)
                    .flex_none()
                    .id("toggle-web-search-key")
                    .debug_selector(|| "toggle-web-search-key".into())
                    .hover(move |style| widgets::ghost_hover(&hover_theme, style))
                    .on_click(cx.listener(|group, _, _, cx| group.toggle_key(cx)))
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

    fn render_body(
        &self,
        theme: &Theme,
        state: &WebSearchSettingsState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.shown();
        let label = shown
            .as_deref()
            .and_then(|id| self.backend(id))
            .map(|backend| backend.name.clone())
            .unwrap_or_else(|| "Off".into());
        let menu = self.render_menu(theme, state, shown.as_deref(), cx);
        let mut rows = group_rows().child(
            group_row()
                .child(row_text(
                    theme,
                    "Search service",
                    "The service the agent searches with.",
                ))
                .child(self.render_trigger(theme, label, menu, cx)),
        );
        if self.key_kind().is_some() {
            let mut actions = Vec::new();
            if self.pending.is_some() {
                let cancel_theme = theme.clone();
                actions.push(
                    widgets::ghost_action(theme)
                        .id("cancel-web-search")
                        .debug_selector(|| "cancel-web-search".into())
                        .hover(move |style| widgets::ghost_hover(&cancel_theme, style))
                        .on_click(cx.listener(|group, _, _, cx| group.cancel_pending(cx)))
                        .child("Cancel")
                        .into_any_element(),
                );
            } else {
                actions.push(
                    remove_button(theme, "remove-web-search")
                        .on_click(cx.listener(|group, _, _, cx| group.remove(cx)))
                        .child("Remove")
                        .into_any_element(),
                );
            }
            actions.push(
                save_button(theme, "save-web-search")
                    .on_click(cx.listener(|group, _, _, cx| group.save_key(cx)))
                    .child("Save")
                    .into_any_element(),
            );
            rows = rows.child(
                group_row()
                    .items_start()
                    .child(row_text(
                        theme,
                        "API key",
                        "Stored on this device, separate from your provider keys.",
                    ))
                    .child(control_with_actions(
                        self.render_key_field(theme, cx),
                        actions,
                    )),
            );
        }
        let mut column = div().flex().flex_col().child(rows);
        if let Some(error) = self.error.clone() {
            column = column.child(
                widgets::error_strip(theme, error)
                    .id("web-search-error")
                    .debug_selector(|| "web-search-error".into()),
            );
        }
        column.into_any_element()
    }
}

impl Render for WebSearchGroup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match &self.web_search {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("web-search-skeleton", &theme, 1, cx.entity_id(), cx)
            }
            Loadable::Error(error) => widgets::error_strip(&theme, error.clone())
                .id("web-search-unavailable")
                .debug_selector(|| "web-search-unavailable".into())
                .into_any_element(),
            Loadable::Ready(state) => {
                let state = state.clone();
                self.render_body(&theme, &state, cx)
            }
        };
        div()
            .id("web-search-group")
            .mt(px(GROUP_GAP))
            .child(group_header(
                &theme,
                "Web search",
                None,
                "Lets the agent search the web. Exa works without a key; the others use your \
                 own API key.",
            ))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    fn backends() -> Vec<WebSearchBackendOption> {
        [
            ("exa", "Exa", false),
            ("zhipu", "Zhipu", true),
            ("bocha", "Bocha", true),
            ("brave", "Brave", true),
        ]
        .into_iter()
        .map(|(id, name, needs_key)| WebSearchBackendOption {
            id: id.into(),
            name: name.into(),
            needs_key,
        })
        .collect()
    }

    fn entry(id: &str, key: &str) -> WebSearchEntryView {
        let name = backends()
            .into_iter()
            .find(|backend| backend.id == id)
            .map_or_else(|| id.to_string(), |backend| backend.name);
        WebSearchEntryView {
            id: id.into(),
            kind: id.into(),
            name,
            api_key_masked: (!key.is_empty()).then(|| masked(key)),
        }
    }

    fn search_state(
        active: Option<&str>,
        entries: Vec<WebSearchEntryView>,
    ) -> WebSearchSettingsState {
        WebSearchSettingsState {
            active: active.map(str::to_string),
            entries,
            backends: backends(),
        }
    }

    const ZHIPU_KEY: &str = "sk-1234567890abcdef";

    /// Exa (keyless) and Zhipu saved; Zhipu active.
    fn zhipu_active() -> WebSearchSettingsState {
        search_state(
            Some("zhipu"),
            vec![entry("exa", ""), entry("zhipu", ZHIPU_KEY)],
        )
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

    /// The group's engine seam: the web-search methods, with the engine's
    /// validation and activation rules.
    struct FakeEngine {
        state: std::sync::Mutex<WebSearchSettingsState>,
        /// Stored keys by entry id.
        keys: std::sync::Mutex<std::collections::HashMap<String, String>>,
        saved: std::sync::Mutex<Vec<serde_json::Value>>,
        /// False stands in for an engine that predates the web-search RPCs.
        available: bool,
    }

    impl FakeEngine {
        fn save(&self, params: serde_json::Value) -> Result<(), holt_rpc::RpcError> {
            use holt_rpc::RpcError;
            let field = |name: &str| {
                params
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let (kind, api_key) = (field("kind"), field("apiKey"));
            let mut state = self.state.lock().unwrap();
            let Some(backend) = state.backends.iter().find(|backend| backend.id == kind) else {
                return Err(RpcError::BadParams(format!(
                    "unknown search backend {kind:?}"
                )));
            };
            if backend.needs_key && api_key.is_empty() {
                return Err(RpcError::BadParams("apiKey is required".into()));
            }
            let view = entry(&kind, &api_key);
            self.saved.lock().unwrap().push(params);
            self.keys.lock().unwrap().insert(view.id.clone(), api_key);
            state.active = Some(view.id.clone());
            match state.entries.iter_mut().find(|stored| stored.id == view.id) {
                Some(stored) => *stored = view,
                None => state.entries.push(view),
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl holt_rpc::RpcService for FakeEngine {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
            use holt_rpc::{RpcError, RpcReply};
            if !self.available {
                return Err(RpcError::UnknownMethod(method.to_string()));
            }
            let id = params
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            match method {
                methods::GET_WEB_SEARCH_SETTINGS => RpcReply::value(&*self.state.lock().unwrap()),
                methods::REVEAL_WEB_SEARCH_KEY => RpcReply::value(&serde_json::json!({
                    "key": self.keys.lock().unwrap().get(&id.unwrap_or_default()).cloned(),
                })),
                methods::SAVE_WEB_SEARCH_BACKEND => {
                    self.save(params)?;
                    RpcReply::value(&*self.state.lock().unwrap())
                }
                methods::SET_ACTIVE_WEB_SEARCH_BACKEND => {
                    let mut state = self.state.lock().unwrap();
                    if let Some(id) = &id
                        && !state.entries.iter().any(|entry| &entry.id == id)
                    {
                        return Err(RpcError::BadParams(format!(
                            "no search backend with id {id:?}"
                        )));
                    }
                    state.active = id;
                    RpcReply::value(&*state)
                }
                methods::REMOVE_WEB_SEARCH_BACKEND => {
                    let id = id.unwrap_or_default();
                    let mut state = self.state.lock().unwrap();
                    state.entries.retain(|entry| entry.id != id);
                    if state.active.as_deref() == Some(id.as_str()) {
                        state.active = None;
                    }
                    self.keys.lock().unwrap().remove(&id);
                    RpcReply::value(&*state)
                }
                _ => Err(RpcError::UnknownMethod(method.to_string())),
            }
        }
    }

    struct Harness<'a> {
        group: Entity<WebSearchGroup>,
        visual: &'a mut gpui::VisualTestContext,
        engine: std::sync::Arc<FakeEngine>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl Harness<'_> {
        /// Drive RPC dispatch a few rounds (test thread), then the gpui
        /// foreground executor.
        fn pump(&self) {
            for _ in 0..6 {
                self.runtime
                    .block_on(async { tokio::task::yield_now().await });
                self.visual.run_until_parked();
            }
        }

        fn update(&mut self, f: impl FnOnce(&mut WebSearchGroup, &mut Context<WebSearchGroup>)) {
            self.group.update(&mut *self.visual, f);
            self.pump();
        }

        fn read<T>(&self, f: impl FnOnce(&WebSearchGroup, &App) -> T) -> T {
            self.visual.read(|cx| f(self.group.read(cx), cx))
        }

        fn pick(&mut self, id: Option<&str>) {
            let id = id.map(str::to_string);
            self.update(|group, cx| group.pick(id, cx));
        }

        fn key_text(&self) -> String {
            self.read(|group, cx| group.key.read(cx).text().to_string())
        }

        fn key_masked(&self) -> bool {
            self.read(|group, cx| group.key.read(cx).is_masked())
        }

        fn type_key(&mut self, text: &str) {
            self.update(|group, cx| group.key.update(cx, |input, cx| input.set_text(text, cx)));
        }

        fn error(&self) -> Option<String> {
            self.read(|group, _| group.error.clone())
        }

        fn active(&self) -> Option<String> {
            self.engine.state.lock().unwrap().active.clone()
        }

        fn saved(&self) -> Vec<serde_json::Value> {
            self.engine.saved.lock().unwrap().clone()
        }

        fn renders(&mut self, selector: &'static str) -> bool {
            self.visual.debug_bounds(selector).is_some()
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

    fn harness<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: WebSearchSettingsState,
        keys: &[(&str, &str)],
    ) -> Harness<'a> {
        harness_with(cx, state, keys, true)
    }

    fn harness_with<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: WebSearchSettingsState,
        keys: &[(&str, &str)],
        available: bool,
    ) -> Harness<'a> {
        let engine = std::sync::Arc::new(FakeEngine {
            state: std::sync::Mutex::new(state),
            keys: std::sync::Mutex::new(
                keys.iter()
                    .map(|(id, key)| (id.to_string(), key.to_string()))
                    .collect(),
            ),
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
        let (group, visual) =
            cx.add_window_view(|_window, cx| WebSearchGroup::new(app_state.clone(), cx));
        let harness = Harness {
            group,
            visual,
            engine,
            runtime,
            _dir: dir,
        };
        harness.pump();
        harness
    }

    /// The dropdown opens from its trigger, lists Off then every backend,
    /// and a pick lands through it.
    #[gpui::test]
    fn the_dropdown_lists_off_then_the_backends(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
        assert!(!harness.renders("web-search-option-off"));
        harness.click("web-search-service");
        for option in [
            "web-search-option-off",
            "web-search-option-exa",
            "web-search-option-zhipu",
            "web-search-option-bocha",
            "web-search-option-brave",
        ] {
            assert!(harness.renders(option), "{option} renders");
        }
        harness.click("web-search-option-off");
        assert_eq!(harness.active(), None);
        assert!(!harness.renders("web-search-key-field"));
    }

    /// A saved service switches at once, Off keeps the entries, and a
    /// keyless service without an entry is saved (and so activated) on pick.
    #[gpui::test]
    fn picks_switch_turn_off_and_save_keyless(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(
            cx,
            search_state(
                Some("exa"),
                vec![entry("exa", ""), entry("zhipu", ZHIPU_KEY)],
            ),
            &[("zhipu", ZHIPU_KEY)],
        );
        // Keyless: no key field.
        assert!(!harness.renders("web-search-key-field"));

        harness.pick(Some("zhipu"));
        assert_eq!(harness.active().as_deref(), Some("zhipu"));
        assert!(harness.renders("web-search-key-field"));
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));
        assert!(!harness.key_masked());

        harness.pick(None);
        assert_eq!(harness.active(), None);
        assert_eq!(harness.engine.state.lock().unwrap().entries.len(), 2);

        harness.pick(Some("exa"));
        assert_eq!(harness.active().as_deref(), Some("exa"));
        assert!(
            harness.saved().is_empty(),
            "an existing entry only switches"
        );

        // Without an entry, a keyless pick saves one.
        harness.update(|group, cx| {
            group.call(
                methods::REMOVE_WEB_SEARCH_BACKEND,
                serde_json::json!({ "id": "exa" }),
                cx,
            )
        });
        harness.pick(Some("exa"));
        assert_eq!(harness.saved(), [serde_json::json!({ "kind": "exa" })]);
        assert_eq!(harness.active().as_deref(), Some("exa"));
    }

    /// A keyed service without a key asks for one before anything
    /// switches; Cancel backs out, Save activates it.
    #[gpui::test]
    fn a_keyed_service_asks_for_its_key_first(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
        harness.pick(Some("bocha"));
        assert_eq!(harness.active().as_deref(), Some("zhipu"));
        assert_eq!(harness.key_text(), "");
        assert!(harness.renders("cancel-web-search"));
        assert!(!harness.renders("remove-web-search"));

        harness.click("cancel-web-search");
        assert_eq!(harness.read(|group, _| group.pending.clone()), None);
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));

        harness.pick(Some("brave"));
        harness.click("save-web-search");
        assert_eq!(harness.error().as_deref(), Some("Enter an API key"));
        assert!(harness.saved().is_empty());

        harness.type_key("brave-key-000000");
        assert!(
            harness.key_masked(),
            "a pasted key is concealed the moment it stops being the masked display"
        );
        harness.click("save-web-search");
        assert_eq!(
            harness.saved(),
            [serde_json::json!({ "kind": "brave", "apiKey": "brave-key-000000" })]
        );
        assert_eq!(harness.active().as_deref(), Some("brave"));
        assert_eq!(harness.error(), None);
        assert_eq!(harness.key_text(), masked("brave-key-000000"));
        assert!(harness.renders("remove-web-search"));
    }

    /// The active service's stored key: an untouched Save writes nothing,
    /// the eye reveals and conceals, an edit saves, Remove turns search off.
    #[gpui::test]
    fn a_stored_key_reveals_edits_and_removes(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
        harness.click("save-web-search");
        assert!(harness.saved().is_empty());

        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), ZHIPU_KEY);
        assert!(!harness.key_masked());
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));

        // Editing a revealed key makes it a draft: the eye then only flips
        // the projection.
        harness.click("toggle-web-search-key");
        harness.type_key("zhipu-key-new-0000");
        assert!(harness.key_masked());
        harness.click("toggle-web-search-key");
        assert!(!harness.key_masked());

        harness.click("save-web-search");
        assert_eq!(
            harness.saved(),
            [serde_json::json!({ "kind": "zhipu", "apiKey": "zhipu-key-new-0000" })]
        );
        assert_eq!(harness.key_text(), masked("zhipu-key-new-0000"));

        harness.click("remove-web-search");
        assert_eq!(harness.active(), None);
        assert!(!harness.renders("web-search-key-field"));
        assert_eq!(harness.engine.state.lock().unwrap().entries.len(), 1);
    }

    /// A reveal answered after the field moved to another service is
    /// dropped: Zhipu's key must not land in Brave's field.
    #[gpui::test]
    fn a_reveal_answered_after_the_pick_moves_is_dropped(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
        harness.update(|group, cx| {
            group.reveal_key(cx);
            group.pick(Some("brave".into()), cx);
        });
        assert_eq!(harness.key_text(), "");
        assert_eq!(harness.error(), None);
    }

    /// Layout invariant: the in-field eye toggle stays inside the key
    /// field's box.
    #[gpui::test]
    fn the_eye_toggle_stays_inside_the_key_field(cx: &mut gpui::TestAppContext) {
        let harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
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

    /// A rejected save surfaces inline and the draft stays in the field.
    #[gpui::test]
    fn a_rejected_save_surfaces_inline(cx: &mut gpui::TestAppContext) {
        let mut harness = harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)]);
        // Force a kind the engine does not offer — the save RPC's validation.
        harness.update(|group, cx| {
            group.pending = Some("bogus".into());
            group.reset_key_field(cx);
        });
        harness.type_key("some-key-000000");
        harness.update(|group, cx| group.save_key(cx));
        assert!(harness.saved().is_empty());
        let error = harness.error();
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("unknown search backend")),
            "unexpected error: {error:?}"
        );
        assert!(harness.renders("web-search-error"));
        assert_eq!(harness.key_text(), "some-key-000000");
    }

    /// An engine without the web-search RPCs reads as version skew — a
    /// named message, not a raw error and not a silent empty group.
    #[gpui::test]
    fn an_engine_without_the_web_search_rpcs_names_the_skew(cx: &mut gpui::TestAppContext) {
        let mut harness = harness_with(cx, search_state(None, vec![]), &[], false);
        assert!(harness.renders("web-search-unavailable"));
        let state = harness.read(|group, _| group.web_search.clone());
        assert!(
            matches!(state, Loadable::Error(ref message) if message.contains("aren't available")),
            "unexpected state: {state:?}"
        );
        assert!(!harness.renders("web-search-service"));
    }
}
