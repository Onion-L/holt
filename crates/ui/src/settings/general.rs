//! The General settings page (automatic-chat-titles ticket 02): everyday
//! preferences that aren't tied to a provider or the agent runtime, starting
//! with the engine-owned title-task settings. The user picks an optional
//! provider-qualified model (empty = automatic titles disabled) and edits the
//! instruction's style notes, both read and saved only through typed RPC —
//! the engine owns `title-settings.json`, validation, and the
//! missing-credentials warning.
//!
//! It also hosts the Web search group (web-tools ticket 07): the user's
//! configured search backends — built-in vendors, each with its own key,
//! and search tools on `mcp.json` servers — one active at a time, read and
//! saved through the web-search RPCs. The backend is the user's choice — a same-vendor
//! provider key only ever surfaces as a display-only hint.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, MouseButton, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_proto::{
    JevSettingsState, Model, Provider, TitleSettingsState, WebSearchEntryView,
    WebSearchSettingsState,
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

/// The MCP search tool kind — and the picker id of an MCP entry not saved
/// yet (saved ones carry a generated `mcp-…` id).
const MCP_KIND: &str = "mcp";

/// One rendered backend row in the Web search picker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendRow {
    /// A built-in kind (which is also its entry id), a saved MCP entry's
    /// id, or [`MCP_KIND`] for a new one.
    id: String,
    name: String,
    /// Settings copy: a built-in's access requirement, or what an MCP row
    /// is.
    note: Option<String>,
    selected: bool,
    /// The entry the next Turn mounts.
    active: bool,
}

/// The picker's rows: every built-in backend the engine offers, in engine
/// order, then the saved MCP search tools, then a row for a new one — with the pick and the active entry marked. No pick leaves
/// none selected — the picker starts on no backend, never a default vendor.
fn backend_rows(state: &WebSearchSettingsState, pick: Option<&str>) -> Vec<BackendRow> {
    let builtins = state.backends.iter().map(|backend| {
        (
            backend.id.clone(),
            backend.name.clone(),
            backend.note.clone(),
        )
    });
    let mcps = state
        .entries
        .iter()
        .filter(|entry| entry.kind == MCP_KIND)
        .map(|entry| {
            (
                entry.id.clone(),
                format!(
                    "{} / {}",
                    entry.server.as_deref().unwrap_or_default(),
                    entry.tool.as_deref().unwrap_or_default()
                ),
                Some("MCP tool".to_string()),
            )
        });
    let new_mcp = std::iter::once((
        MCP_KIND.to_string(),
        "New MCP search tool".to_string(),
        Some("A tool on a server in mcp.json".to_string()),
    ));
    builtins
        .chain(mcps)
        .chain(new_mcp)
        .map(|(id, name, note)| BackendRow {
            selected: pick == Some(id.as_str()),
            active: state.active.as_deref() == Some(id.as_str()),
            id,
            name,
            note,
        })
        .collect()
}

/// The MCP entry's two menus: its `mcp.json` server and that server's tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpMenu {
    Server,
    Tool,
}

impl McpMenu {
    fn id(self) -> &'static str {
        match self {
            McpMenu::Server => "web-search-server",
            McpMenu::Tool => "web-search-tool",
        }
    }
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
    /// The picker's row id (see [`BackendRow::id`]) — the active entry
    /// until the user picks again. The fields below edit this pick.
    web_search_pick: Option<String>,
    web_search_key: Entity<ComposerInput>,
    /// An MCP entry's `mcp.json` server, picked from `mcp_servers`.
    web_search_server: Option<String>,
    /// An MCP entry's search tool: set from the tool menu, or typed when
    /// the server's tools can't be listed.
    web_search_tool: Entity<ComposerInput>,
    /// Server names from `GetMcpSettings`, read when the server menu opens.
    mcp_servers: Loadable<Vec<String>>,
    /// The picked server's tool names from `TestMcpServer`, read when the
    /// tool menu opens. An error swaps the menu for a text field.
    mcp_tools: Loadable<Vec<String>>,
    /// The server `mcp_tools` belongs to.
    mcp_tools_server: Option<String>,
    mcp_servers_task: Option<Task<()>>,
    mcp_tools_task: Option<Task<()>>,
    /// The raw stored key the field currently shows, fetched by
    /// `RevealWebSearchKey`; `None` while it shows the engine's masked
    /// display or the user's draft.
    web_search_revealed_key: Option<String>,
    /// Draft-only projection: the eye hides the key the user is typing.
    /// Concealed by default, like the provider-key rows.
    web_search_draft_concealed: bool,
    web_search_error: Option<String>,
    backend_menu: Popup<()>,
    server_menu: Popup<()>,
    tool_menu: Popup<()>,
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
    /// Re-derives the key field's projection on every edit: a pasted key is
    /// concealed the moment it stops being the masked display.
    _key_events: Subscription,
    /// Same for the Jev key field.
    _jev_key_events: Subscription,
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
        let jev_key = cx.new(|cx| ComposerInput::new_secret("TypeSafe API key", cx));
        let jev_key_events = cx.subscribe(
            &jev_key,
            |page: &mut Self, _, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    page.sync_jev_mask(cx);
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
            web_search_pick: None,
            web_search_key,
            web_search_server: None,
            web_search_tool: cx.new(|cx| ComposerInput::new("Tool name", cx)),
            mcp_servers: Loadable::Idle,
            mcp_tools: Loadable::Idle,
            mcp_tools_server: None,
            mcp_servers_task: None,
            mcp_tools_task: None,
            web_search_revealed_key: None,
            web_search_draft_concealed: true,
            web_search_error: None,
            backend_menu: Popup::default(),
            server_menu: Popup::default(),
            tool_menu: Popup::default(),
            jev: Loadable::Idle,
            jev_key,
            jev_revealed_key: None,
            jev_draft_concealed: true,
            jev_error: None,
            _key_events: key_events,
            _jev_key_events: jev_key_events,
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
            .picked_entry()
            .and_then(|entry| entry.api_key_masked.as_deref());
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

    /// Echo a web-search reply (read, save, set-active, or remove) into
    /// the group's editable state: the pick returns to the active entry and
    /// its fields reset to the stored truth.
    fn apply_web_search_state(&mut self, state: WebSearchSettingsState, cx: &mut Context<Self>) {
        self.web_search_pick = state.active.clone();
        self.web_search = Loadable::Ready(state);
        self.load_pick_fields(cx);
    }

    /// The saved entry the pick names; `None` for an unsaved built-in or a
    /// new MCP entry.
    fn picked_entry(&self) -> Option<&WebSearchEntryView> {
        let pick = self.web_search_pick.as_deref()?;
        self.web_search
            .ready()?
            .entries
            .iter()
            .find(|entry| entry.id == pick)
    }

    /// The pick's backend kind: the saved entry's, or the pick itself (a
    /// built-in id, or [`MCP_KIND`]).
    fn picked_kind(&self) -> Option<String> {
        match self.picked_entry() {
            Some(entry) => Some(entry.kind.clone()),
            None => self.web_search_pick.clone(),
        }
    }

    /// Fill the fields from the picked entry — its masked key, server, and
    /// tool — or clear them for an unsaved pick. The pick is set first: the
    /// edit `set_text` emits re-derives the key projection against it.
    fn load_pick_fields(&mut self, cx: &mut Context<Self>) {
        let entry = self.picked_entry().cloned();
        let masked = entry
            .as_ref()
            .and_then(|entry| entry.api_key_masked.clone())
            .unwrap_or_default();
        let server = entry.as_ref().and_then(|entry| entry.server.clone());
        let tool = entry
            .as_ref()
            .and_then(|entry| entry.tool.clone())
            .unwrap_or_default();
        self.web_search_revealed_key = None;
        self.web_search_draft_concealed = true;
        self.web_search_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
        self.web_search_tool
            .update(cx, |input, cx| input.set_text(tool, cx));
        self.set_web_search_server(server);
    }

    /// Point the MCP fields at `server`; its tool list is read again on
    /// the next tool-menu open.
    fn set_web_search_server(&mut self, server: Option<String>) {
        if server != self.mcp_tools_server {
            self.mcp_tools = Loadable::Idle;
            self.mcp_tools_server = None;
            self.mcp_tools_task = None;
        }
        self.web_search_server = server;
    }

    /// Pick a server from the menu. A new server clears the tool (it
    /// belonged to the old one); re-picking one retries its tool list.
    fn pick_mcp_server(&mut self, server: String, cx: &mut Context<Self>) {
        if self.web_search_server.as_ref() != Some(&server) {
            self.web_search_tool
                .update(cx, |input, cx| input.set_text("", cx));
        }
        self.set_web_search_server(None);
        self.set_web_search_server(Some(server));
        self.web_search_error = None;
        cx.notify();
    }

    fn pick_mcp_tool(&mut self, tool: String, cx: &mut Context<Self>) {
        self.web_search_tool
            .update(cx, |input, cx| input.set_text(tool, cx));
        self.web_search_error = None;
        cx.notify();
    }

    /// Read the `mcp.json` server names, unless already read.
    fn load_mcp_servers(&mut self, cx: &mut Context<Self>) {
        if matches!(self.mcp_servers, Loadable::Loading | Loadable::Ready(_)) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.mcp_servers = Loadable::Error("Engine not connected".into());
            return;
        };
        self.mcp_servers = Loadable::Loading;
        self.mcp_servers_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_MCP_SETTINGS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.mcp_servers = match result {
                    Ok(value) => Loadable::Ready(
                        value["servers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|server| server["name"].as_str().map(str::to_string))
                            .collect(),
                    ),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// List the picked server's tools through `TestMcpServer`, unless
    /// already read for it.
    fn load_mcp_tools(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.web_search_server.clone() else {
            return;
        };
        if self.mcp_tools_server.as_ref() == Some(&server) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.mcp_tools = Loadable::Error("Engine not connected".into());
            return;
        };
        self.mcp_tools = Loadable::Loading;
        self.mcp_tools_server = Some(server.clone());
        self.mcp_tools_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::TEST_MCP_SERVER,
                    serde_json::json!({ "name": server }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.mcp_tools = match result {
                    Ok(value) if value["status"] == "ok" => Loadable::Ready(
                        value["toolNames"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|tool| tool.as_str().map(str::to_string))
                            .collect(),
                    ),
                    Ok(value) => Loadable::Error(
                        value["reason"]
                            .as_str()
                            .unwrap_or("the server didn't answer")
                            .to_string(),
                    ),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                // Nothing to pick from: fall back to typing the name.
                if page.tool_menu.is_open() && page.mcp_tools.ready().is_none() {
                    page.close_mcp_menu(McpMenu::Tool, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Pick a picker row. A saved entry becomes active right away; an
    /// unsaved one only loads empty fields until Save.
    fn pick_web_search_backend(&mut self, id: String, cx: &mut Context<Self>) {
        self.web_search_pick = Some(id.clone());
        self.web_search_error = None;
        self.load_pick_fields(cx);
        let switch = self.web_search.ready().is_some_and(|state| {
            state.active.as_deref() != Some(id.as_str())
                && state.entries.iter().any(|entry| entry.id == id)
        });
        if switch {
            self.call_web_search(
                methods::SET_ACTIVE_WEB_SEARCH_BACKEND,
                serde_json::json!({ "id": id }),
                cx,
            );
        }
        cx.notify();
    }

    /// Send a web-search RPC that replies the settings state, and echo the
    /// reply (or the error) into the group.
    fn call_web_search(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.apply_web_search_reply(result, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn apply_web_search_reply(
        &mut self,
        result: Result<serde_json::Value, holt_rpc::RpcError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(value) => match serde_json::from_value::<WebSearchSettingsState>(value) {
                Ok(state) => {
                    self.apply_web_search_state(state, cx);
                    self.web_search_error = None;
                }
                Err(error) => self.web_search_error = Some(error.to_string()),
            },
            Err(error) => self.web_search_error = Some(error.to_string()),
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.settings = Loadable::Error("Engine not connected".into());
            self.models = Loadable::Error("Engine not connected".into());
            self.web_search = Loadable::Error("Engine not connected".into());
            self.jev = Loadable::Error("Engine not connected".into());
            return;
        };
        self.settings = Loadable::Loading;
        self.models = Loadable::Loading;
        self.web_search = Loadable::Loading;
        self.jev = Loadable::Loading;
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
        let Some(id) = self.picked_entry().map(|entry| entry.id.clone()) else {
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
            this.update(cx, |page, cx| {
                // The user picked another row meanwhile: the key is not
                // this field's to show.
                if page.web_search_pick.as_deref() != Some(id.as_str()) {
                    return;
                }
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
            .picked_entry()
            .and_then(|entry| entry.api_key_masked.clone())
            .unwrap_or_default();
        self.web_search_key.update(cx, |input, cx| {
            input.set_masked(false, cx);
            input.set_text(masked, cx);
        });
        self.web_search_revealed_key = None;
        self.web_search_draft_concealed = true;
        cx.notify();
    }

    /// Save the pick: a built-in's key, or an MCP entry's server and tool.
    /// The engine makes the saved entry active. An
    /// untouched key field holds the engine's masked display, never a
    /// usable key: the picked entry's stored key is re-read through
    /// `RevealWebSearchKey` and re-saved instead.
    fn save_web_search(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.web_search_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let Some(kind) = self.picked_kind() else {
            self.web_search_error = Some("Choose a search backend".into());
            cx.notify();
            return;
        };
        let entry_id = self.picked_entry().map(|entry| entry.id.clone());
        if kind == MCP_KIND {
            let server = self.web_search_server.clone().unwrap_or_default();
            let tool = self.web_search_tool.read(cx).text().trim().to_string();
            let missing = if server.is_empty() {
                Some("Choose an MCP server")
            } else if tool.is_empty() {
                Some("Choose a tool")
            } else {
                None
            };
            if let Some(message) = missing {
                self.web_search_error = Some(message.into());
                cx.notify();
                return;
            }
            let mut params = serde_json::json!({ "kind": kind, "server": server, "tool": tool });
            if let Some(id) = entry_id {
                params["id"] = id.into();
            }
            self.call_web_search(methods::SAVE_WEB_SEARCH_BACKEND, params, cx);
            return;
        }
        let mut params = serde_json::json!({ "kind": kind });
        let draft = self.web_search_key.read(cx).text().to_string();
        let untouched = self.key_field_state(cx) == KeyField::Stored;
        self.task = Some(cx.spawn(async move |this, cx| {
            let key = if untouched {
                match engine
                    .client()
                    .call(
                        methods::REVEAL_WEB_SEARCH_KEY,
                        serde_json::json!({ "id": entry_id }),
                    )
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
            params["apiKey"] = key.into();
            let result = engine
                .client()
                .call(methods::SAVE_WEB_SEARCH_BACKEND, params)
                .await;
            this.update(cx, |page, cx| {
                page.apply_web_search_reply(result, cx);
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

    /// Remove the picked entry; removing the active one leaves the agent
    /// without a web search tool.
    fn remove_web_search(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.picked_entry().map(|entry| entry.id.clone()) else {
            return;
        };
        self.call_web_search(
            methods::REMOVE_WEB_SEARCH_BACKEND,
            serde_json::json!({ "id": id }),
            cx,
        );
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

    fn mcp_menu(&mut self, menu: McpMenu) -> &mut Popup<()> {
        match menu {
            McpMenu::Server => &mut self.server_menu,
            McpMenu::Tool => &mut self.tool_menu,
        }
    }

    fn close_mcp_menu(&mut self, menu: McpMenu, cx: &mut Context<Self>) {
        if self.mcp_menu(menu).begin_close() {
            popover::reap_popup(cx, move |page: &mut Self| page.mcp_menu(menu));
        }
    }

    /// Open a Server/Tool menu, reading its options on the way. The tool
    /// menu waits for a server.
    fn toggle_mcp_menu(&mut self, menu: McpMenu, cx: &mut Context<Self>) {
        let popup = self.mcp_menu(menu);
        if popup.take_press_was_open() || popup.is_open() {
            self.close_mcp_menu(menu, cx);
        } else {
            match menu {
                McpMenu::Server => self.load_mcp_servers(cx),
                McpMenu::Tool if self.web_search_server.is_some() => self.load_mcp_tools(cx),
                McpMenu::Tool => return,
            }
            self.mcp_menu(menu).open(());
        }
        cx.notify();
    }

    fn pick_mcp_option(&mut self, menu: McpMenu, value: String, cx: &mut Context<Self>) {
        self.close_mcp_menu(menu, cx);
        match menu {
            McpMenu::Server => self.pick_mcp_server(value, cx),
            McpMenu::Tool => self.pick_mcp_tool(value, cx),
        }
    }

    /// The Server menu: `mcp.json`'s servers, plus the stored one when the
    /// file no longer defines it.
    fn mcp_server_dropdown(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.web_search_server.clone();
        let (mut options, empty) = match &self.mcp_servers {
            Loadable::Ready(servers) => (servers.clone(), "No servers in mcp.json".to_string()),
            Loadable::Error(error) => (Vec::new(), format!("Couldn't read mcp.json: {error}")),
            Loadable::Idle | Loadable::Loading => (Vec::new(), "Loading servers…".to_string()),
        };
        if let Some(current) = &current
            && self.mcp_servers.ready().is_some()
            && !options.contains(current)
        {
            options.push(current.clone());
        }
        let label = current.unwrap_or_else(|| "Select a server".to_string());
        self.mcp_dropdown(theme, McpMenu::Server, label, options, empty, cx)
    }

    /// The Tool menu: the picked server's tools, plus the stored one when
    /// the server no longer lists it.
    fn mcp_tool_dropdown(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let tool = self.web_search_tool.read(cx).text().trim().to_string();
        let (mut options, empty) = match &self.mcp_tools {
            Loadable::Ready(tools) => (tools.clone(), "No tools on this server".to_string()),
            _ => (Vec::new(), "Loading tools…".to_string()),
        };
        if !tool.is_empty() && self.mcp_tools.ready().is_some() && !options.contains(&tool) {
            options.push(tool.clone());
        }
        let label = match (tool.is_empty(), self.web_search_server.is_some()) {
            (false, _) => tool,
            (true, true) => "Select a tool".to_string(),
            (true, false) => "Select a server first".to_string(),
        };
        self.mcp_dropdown(theme, McpMenu::Tool, label, options, empty, cx)
    }

    /// A Server/Tool menu trigger, styled like the Backend one. With no
    /// `options` the menu shows `empty` as its one inert row.
    fn mcp_dropdown(
        &self,
        theme: &Theme,
        menu: McpMenu,
        label: String,
        options: Vec<String>,
        empty: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = menu.id();
        let popup = match menu {
            McpMenu::Server => &self.server_menu,
            McpMenu::Tool => &self.tool_menu,
        };
        let rows: Vec<AnyElement> = if options.is_empty() {
            vec![
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(empty))
                    .into_any_element(),
            ]
        } else {
            options
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    let selected = option == label;
                    let row_id = format!("{id}-option-{index}");
                    let selector = row_id.clone();
                    popover::menu_row(theme, selected, row_id.clone())
                        .id(SharedString::from(row_id))
                        .debug_selector(move || selector.clone())
                        .on_click(cx.listener({
                            let option = option.clone();
                            move |page, _, _, cx| {
                                cx.stop_propagation();
                                page.pick_mcp_option(menu, option.clone(), cx);
                            }
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(option)),
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
                .collect()
        };
        let card = popover::popover_card(theme)
            .id(SharedString::from(format!("{id}-scroll")))
            .w(px(CONTROL_WIDTH))
            .max_h(px(240.0))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(move |page, _, _, cx| page.close_mcp_menu(menu, cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows)
            .into_any_element();
        div()
            .id(id)
            .debug_selector(move || id.into())
            .relative()
            .flex_none()
            .w(px(CONTROL_WIDTH))
            .h(px(CONTROL_HEIGHT))
            .px(px(10.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(if popup.is_open() {
                theme.ink(0.09)
            } else {
                theme.ink(0.05)
            })
            .when(!popup.is_open(), |el| {
                el.hover(|style| style.bg(crate::theme::ink(0.07)))
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |page, _, _, _| page.mcp_menu(menu).note_trigger_press()),
            )
            .on_click(cx.listener(move |page, _, _, cx| page.toggle_mcp_menu(menu, cx)))
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
            .when_some(popup.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    format!("{id}-menu"),
                    card,
                    popup.closing_since(),
                ))
            })
            .into_any_element()
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
                let picked_kind = self.picked_kind();
                let stored = self.picked_entry().is_some();
                let mcp = picked_kind.as_deref() == Some(MCP_KIND);
                let hint = zhipu_hint_visible(
                    picked_kind.as_deref(),
                    self.providers.ready().map(Vec::as_slice).unwrap_or(&[]),
                );
                let rows = backend_rows(state, self.web_search_pick.as_deref());
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
                    let note = match (&row.note, row.active) {
                        (Some(note), true) => Some(format!("Active · {note}")),
                        (None, true) => Some("Active".to_string()),
                        (note, false) => note.clone(),
                    };
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
                        page.close_backend_menu(cx);
                        page.pick_web_search_backend(id.clone(), cx);
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
                    .flex_none()
                    .w(px(CONTROL_WIDTH))
                    .h(px(CONTROL_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .bg(if self.backend_menu.is_open() {
                        theme.ink(0.09)
                    } else {
                        theme.ink(0.05)
                    })
                    .when(!self.backend_menu.is_open(), |el| {
                        el.hover(|style| style.bg(crate::theme::ink(0.07)))
                    })
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

                let key_text = row_text(
                    theme,
                    "API key",
                    "Stored on this device, separate from your provider keys.",
                )
                .when(hint, |text| {
                    text.child(
                        div()
                            .id("web-search-hint")
                            .debug_selector(|| "web-search-hint".into())
                            .child(widgets::row_description(
                                theme,
                                "An existing Zhipu provider key is configured; it works for \
                                 Zhipu search too.",
                            )),
                    )
                });

                let mut actions = Vec::new();
                if stored {
                    actions.push(
                        remove_button(theme, "remove-web-search")
                            .on_click(cx.listener(|page, _, _, cx| page.remove_web_search(cx)))
                            .child("Remove")
                            .into_any_element(),
                    );
                }
                actions.push(
                    save_button(theme, "save-web-search")
                        .on_click(cx.listener(|page, _, _, cx| page.save_web_search(cx)))
                        .child("Save")
                        .into_any_element(),
                );
                let card = group_rows()
                    .child(
                        group_row()
                            .child(row_text(
                                theme,
                                "Backend",
                                "The search service the agent queries.",
                            ))
                            .child(backend_trigger),
                    )
                    .when(mcp, |card| {
                        card.child(
                            group_row()
                                .child(row_text(
                                    theme,
                                    "Server",
                                    "A server defined in mcp.json; its own auth applies.",
                                ))
                                .child(self.mcp_server_dropdown(theme, cx)),
                        )
                    });
                let card = if mcp {
                    // A server whose tools can't be listed falls back to
                    // typing the tool name, with the reason alongside.
                    let (tool_control, tool_description) = match &self.mcp_tools {
                        Loadable::Error(reason) => (
                            text_field(
                                theme,
                                "web-search-tool-input",
                                self.web_search_tool.clone(),
                            )
                            .into_any_element(),
                            format!("Couldn't list this server's tools ({reason}). Type the name."),
                        ),
                        _ => (
                            self.mcp_tool_dropdown(theme, cx),
                            "Called with the query; its text goes to the agent as-is.".to_string(),
                        ),
                    };
                    card.child(
                        group_row()
                            .items_start()
                            .child(row_text(theme, "Tool", &tool_description))
                            .child(control_with_actions(tool_control, actions)),
                    )
                } else {
                    card.child(group_row().items_start().child(key_text).child(
                        control_with_actions(
                            web_search_key_field(
                                theme,
                                self.web_search_key.clone(),
                                self.key_field_state(cx),
                                self.web_search_draft_concealed,
                                cx,
                            ),
                            actions,
                        ),
                    ))
                };
                let mut column = div().flex().flex_col().child(card);
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
        let status = self
            .web_search
            .ready()
            .map(|state| status_pill(theme, state.active.is_some(), "web-search-unconfigured"));
        div()
            .id("web-search-group")
            .mt(px(GROUP_GAP))
            .child(group_header(
                theme,
                "Web search",
                status,
                "Search services chosen by you — a vendor with its own key, or a search tool on \
                 an MCP server; the agent uses the active one. Without one the agent has no web \
                 search tool; reading a page is a separate tool.",
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

/// A plain text control on the shared control column.
fn text_field(theme: &Theme, id: &'static str, input: Entity<ComposerInput>) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_none()
        .w(px(CONTROL_WIDTH))
        .h(px(CONTROL_HEIGHT))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(Theme::CONTROL_RADIUS))
        .bg(theme.ink(0.05))
        .child(div().flex_1().min_w_0().child(input))
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
                    .child(self.render_web_search(&theme, cx))
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

    // ---- Web search group (web-tools ticket 07) ----

    fn backend_option(
        id: &str,
        name: &str,
        note: Option<&str>,
    ) -> holt_proto::WebSearchBackendOption {
        holt_proto::WebSearchBackendOption {
            id: id.into(),
            name: name.into(),
            note: note.map(str::to_string),
        }
    }

    fn launch_backends() -> Vec<holt_proto::WebSearchBackendOption> {
        vec![
            backend_option("zhipu", "Zhipu", None),
            backend_option("bocha", "Bocha", None),
            backend_option("brave", "Brave", None),
        ]
    }

    fn entry(id: &str, kind: &str, name: &str, key: &str) -> WebSearchEntryView {
        WebSearchEntryView {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            server: None,
            tool: None,
            api_key_masked: (!key.is_empty()).then(|| masked(key)),
        }
    }

    fn mcp_entry(id: &str, server: &str, tool: &str) -> WebSearchEntryView {
        WebSearchEntryView {
            server: Some(server.into()),
            tool: Some(tool.into()),
            ..entry(id, MCP_KIND, MCP_KIND, "")
        }
    }

    /// A settings state over the launch backends.
    fn search_state(
        active: Option<&str>,
        entries: Vec<WebSearchEntryView>,
    ) -> WebSearchSettingsState {
        WebSearchSettingsState {
            active: active.map(str::to_string),
            entries,
            backends: launch_backends(),
        }
    }

    const ZHIPU_KEY: &str = "sk-1234567890abcdef";

    /// Zhipu saved and active.
    fn zhipu_active() -> WebSearchSettingsState {
        search_state(
            Some("zhipu"),
            vec![entry("zhipu", "zhipu", "Zhipu", ZHIPU_KEY)],
        )
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
            custom: false,
            logo: None,
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
    fn backend_rows_list_builtins_then_mcp_entries_then_a_new_one() {
        let state = search_state(
            Some("mcp-1"),
            vec![
                entry("bocha", "bocha", "Bocha", "bocha-key-0000"),
                mcp_entry("mcp-1", "tinyfish", "search"),
            ],
        );
        let rows = backend_rows(&state, Some("bocha"));
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["zhipu", "bocha", "brave", "mcp-1", "mcp"]);
        assert!(rows[1].selected && !rows[1].active);
        assert!(rows[3].active && !rows[3].selected);
        assert_eq!(rows[3].name, "tinyfish / search");
        // No pick: no vendor starts selected.
        assert!(
            backend_rows(&search_state(None, vec![]), None)
                .iter()
                .all(|row| !row.selected && !row.active)
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

    /// The Web search group's engine seam: the web-search methods plus
    /// the provider/title reads the page performs on load. Everything else is
    /// version skew and parks the AppState's standing watches on the retry
    /// timer (the notifications harness' stance).
    struct FakeWebSearchEngine {
        state: std::sync::Mutex<WebSearchSettingsState>,
        /// Stored keys by entry id.
        keys: std::sync::Mutex<std::collections::HashMap<String, String>>,
        jev_state: std::sync::Mutex<JevSettingsState>,
        jev_key: std::sync::Mutex<Option<String>>,
        jev_saved: std::sync::Mutex<Vec<serde_json::Value>>,
        providers: std::sync::Mutex<Vec<Provider>>,
        saved: std::sync::Mutex<Vec<serde_json::Value>>,
        /// False stands in for an engine that predates the web-search RPCs.
        available: bool,
    }

    impl FakeWebSearchEngine {
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
            let view = if kind == MCP_KIND {
                let (server, tool) = (field("server"), field("tool"));
                if server.is_empty() || tool.is_empty() {
                    return Err(RpcError::BadParams("server and tool are required".into()));
                }
                let id = match field("id") {
                    id if id.is_empty() => format!("mcp-{}", state.entries.len() + 1),
                    id => id,
                };
                mcp_entry(&id, &server, &tool)
            } else {
                let Some(option) = state.backends.iter().find(|option| option.id == kind) else {
                    return Err(RpcError::BadParams(format!(
                        "unknown search backend {kind:?}"
                    )));
                };
                if api_key.is_empty() {
                    return Err(RpcError::BadParams("apiKey is required".into()));
                }
                entry(&kind, &kind, &option.name.clone(), &api_key)
            };
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
                        | methods::SAVE_WEB_SEARCH_BACKEND
                        | methods::SET_ACTIVE_WEB_SEARCH_BACKEND
                        | methods::REVEAL_WEB_SEARCH_KEY
                        | methods::REMOVE_WEB_SEARCH_BACKEND
                )
            {
                return Err(RpcError::UnknownMethod(method.to_string()));
            }
            let id = params
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
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
                    "key": self.keys.lock().unwrap().get(&id).cloned(),
                })),
                methods::SAVE_WEB_SEARCH_BACKEND => {
                    self.save(params)?;
                    RpcReply::value(&*self.state.lock().unwrap())
                }
                methods::SET_ACTIVE_WEB_SEARCH_BACKEND => {
                    let mut state = self.state.lock().unwrap();
                    if !state.entries.iter().any(|entry| entry.id == id) {
                        return Err(RpcError::BadParams(format!(
                            "no search backend with id {id:?}"
                        )));
                    }
                    state.active = Some(id);
                    RpcReply::value(&*state)
                }
                methods::REMOVE_WEB_SEARCH_BACKEND => {
                    let mut state = self.state.lock().unwrap();
                    state.entries.retain(|entry| entry.id != id);
                    if state.active.as_deref() == Some(id.as_str()) {
                        state.active = None;
                    }
                    self.keys.lock().unwrap().remove(&id);
                    RpcReply::value(&*state)
                }
                methods::GET_MCP_SETTINGS => RpcReply::value(&serde_json::json!({
                    "servers": [
                        { "name": "tinyfish", "enabled": true },
                        { "name": "broken", "enabled": true },
                    ],
                })),
                methods::TEST_MCP_SERVER => RpcReply::value(&match params["name"].as_str() {
                    Some("tinyfish") => serde_json::json!({
                        "status": "ok",
                        "toolCount": 2,
                        "toolNames": ["search", "web_search"],
                    }),
                    _ => serde_json::json!({ "status": "failed", "reason": "connection refused" }),
                }),
                methods::GET_JEV_SETTINGS => RpcReply::value(&*self.jev_state.lock().unwrap()),
                methods::REVEAL_JEV_KEY => RpcReply::value(&serde_json::json!({
                    "key": self.jev_key.lock().unwrap().clone(),
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
                    self.jev_saved.lock().unwrap().push(params);
                    *self.jev_key.lock().unwrap() = Some(api_key.clone());
                    *self.jev_state.lock().unwrap() = JevSettingsState {
                        api_key_masked: Some(masked(&api_key)),
                    };
                    RpcReply::value(&*self.jev_state.lock().unwrap())
                }
                methods::REMOVE_JEV_SETTINGS => {
                    *self.jev_key.lock().unwrap() = None;
                    *self.jev_state.lock().unwrap() = JevSettingsState {
                        api_key_masked: None,
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

        /// Pick a picker row, as its click does.
        fn pick(&mut self, id: &str) {
            self.page.update(&mut *self.visual, |page, cx| {
                page.pick_web_search_backend(id.to_string(), cx);
            });
            self.pump();
        }

        fn type_into(&mut self, field: fn(&GeneralPage) -> &Entity<ComposerInput>, text: &str) {
            self.page.update(&mut *self.visual, |page, cx| {
                field(page).update(cx, |input, cx| input.set_text(text, cx));
                cx.notify();
            });
            self.pump();
        }

        /// Pick a row and type a key for it.
        fn draft(&mut self, pick: &str, key: &str) {
            self.pick(pick);
            self.type_into(|page| &page.web_search_key, key);
        }

        fn error(&self) -> Option<String> {
            self.visual
                .read(|cx| self.page.read(cx).web_search_error.clone())
        }

        fn active(&self) -> Option<String> {
            self.engine.state.lock().unwrap().active.clone()
        }

        /// Click a menu option, then let the menu's exit animation finish
        /// so it no longer covers the rows below. `finish_close` measures
        /// the exit on the wall clock, the reap timer on the test clock.
        fn choose(&mut self, selector: &'static str) {
            self.click(selector);
            std::thread::sleep(
                crate::motion::MENU_OUT
                    .total()
                    .mul_f32(crate::motion::speed_scale()),
            );
            self.visual
                .executor()
                .advance_clock(std::time::Duration::from_secs(1));
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
        keys: &[(&str, &str)],
        providers: Vec<Provider>,
    ) -> WebSearchHarness<'a> {
        web_search_harness_with(
            cx,
            state,
            keys,
            providers,
            true,
            JevSettingsState {
                api_key_masked: None,
            },
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn web_search_harness_with<'a>(
        cx: &'a mut gpui::TestAppContext,
        state: WebSearchSettingsState,
        keys: &[(&str, &str)],
        providers: Vec<Provider>,
        available: bool,
        jev_state: JevSettingsState,
        jev_key: Option<&str>,
    ) -> WebSearchHarness<'a> {
        let engine = std::sync::Arc::new(FakeWebSearchEngine {
            state: std::sync::Mutex::new(state),
            keys: std::sync::Mutex::new(
                keys.iter()
                    .map(|(id, key)| (id.to_string(), key.to_string()))
                    .collect(),
            ),
            jev_state: std::sync::Mutex::new(jev_state),
            jev_key: std::sync::Mutex::new(jev_key.map(str::to_string)),
            jev_saved: std::sync::Mutex::new(Vec::new()),
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
    /// rides `SaveWebSearchBackend`.
    #[gpui::test]
    fn the_group_shows_the_masked_key_and_saves_through_the_rpc(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(
            cx,
            zhipu_active(),
            &[("zhipu", ZHIPU_KEY)],
            vec![provider("zai", true)],
        );
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));
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
        // Built-ins have no server or tool fields.
        assert!(harness.visual.debug_bounds("web-search-server").is_none());

        // Saving untouched re-reads and re-saves the stored key — the masked
        // display itself is never written as a key.
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[serde_json::json!({ "kind": "zhipu", "apiKey": ZHIPU_KEY })]
        );

        // The eye reveals the stored key through RevealWebSearchKey, then
        // conceals it back to the engine's masked display.
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), ZHIPU_KEY);
        assert!(!harness.key_masked());
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));
        assert!(!harness.key_masked());

        // A revealed key belongs to its entry: picking an unsaved backend
        // clears the field, so saving it asks for that backend's own key.
        harness.click("toggle-web-search-key");
        assert_eq!(harness.key_text(), ZHIPU_KEY);
        harness.pick("brave");
        assert_eq!(harness.key_text(), "");
        assert!(harness.visual.debug_bounds("web-search-hint").is_none());
        harness.click("save-web-search");
        assert_eq!(harness.engine.saved.lock().unwrap().len(), 1);
        assert_eq!(harness.error().as_deref(), Some("Enter an API key"));
        // Picking alone never switched the active backend.
        assert_eq!(harness.active().as_deref(), Some("zhipu"));

        // Editing a revealed key makes it a draft: the eye is then only a
        // projection flip, never a restore that discards what was typed.
        harness.pick("zhipu");
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));
        harness.click("toggle-web-search-key");
        harness.type_into(|page| &page.web_search_key, "typed-after-reveal");
        assert!(harness.key_masked(), "a draft is concealed by default");
        harness.click("toggle-web-search-key");
        assert!(!harness.key_masked());
        assert_eq!(harness.key_text(), "typed-after-reveal");
        harness.click("toggle-web-search-key");
        assert!(harness.key_masked());
        assert_eq!(harness.key_text(), "typed-after-reveal");

        // A draft key saves with the picked backend, which becomes active.
        harness.draft("bocha", "bocha-key-0000");
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[
                serde_json::json!({ "kind": "zhipu", "apiKey": ZHIPU_KEY }),
                serde_json::json!({ "kind": "bocha", "apiKey": "bocha-key-0000" }),
            ]
        );
        assert_eq!(harness.active().as_deref(), Some("bocha"));
        // The save reply re-echoes the masked key and the draft is gone.
        assert_eq!(harness.key_text(), masked("bocha-key-0000"));
        assert!(harness.visual.debug_bounds("web-search-error").is_none());

        // Picking a saved entry switches to it at once.
        harness.pick("zhipu");
        assert_eq!(harness.active().as_deref(), Some("zhipu"));
        assert_eq!(harness.key_text(), masked(ZHIPU_KEY));

        // Removing the active entry leaves the agent without the tool; the
        // other entry stays saved.
        harness.click("remove-web-search");
        assert_eq!(harness.active(), None);
        assert_eq!(harness.engine.state.lock().unwrap().entries.len(), 1);
        assert!(
            harness
                .visual
                .debug_bounds("web-search-unconfigured")
                .is_some()
        );
        assert_eq!(harness.key_text(), "");
    }

    /// An MCP search tool: server and tool rows replace the key field, both
    /// are required, and a saved entry is edited in place by its id.
    /// Server and tool are picked from menus: the servers `GetMcpSettings`
    /// reports, then the tools `TestMcpServer` lists for the picked one.
    #[gpui::test]
    fn mcp_entries_need_a_server_and_tool_and_edit_in_place(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)], vec![]);
        harness.pick(MCP_KIND);
        assert!(harness.visual.debug_bounds("web-search-server").is_some());
        assert!(harness.visual.debug_bounds("web-search-tool").is_some());
        assert!(
            harness
                .visual
                .debug_bounds("web-search-key-field")
                .is_none()
        );
        assert!(harness.visual.debug_bounds("remove-web-search").is_none());

        harness.click("save-web-search");
        assert_eq!(harness.error().as_deref(), Some("Choose an MCP server"));
        harness.click("web-search-server");
        harness.choose("web-search-server-option-0");
        harness.click("save-web-search");
        assert_eq!(harness.error().as_deref(), Some("Choose a tool"));
        harness.click("web-search-tool");
        harness.choose("web-search-tool-option-0");
        harness.click("save-web-search");
        assert_eq!(harness.error(), None);
        assert_eq!(
            harness.engine.saved.lock().unwrap().as_slice(),
            &[serde_json::json!({ "kind": "mcp", "server": "tinyfish", "tool": "search" })]
        );
        // The saved entry is active and picked, its fields echoed back.
        assert_eq!(harness.active().as_deref(), Some("mcp-2"));
        let (pick, server) = harness.visual.read(|cx| {
            let page = harness.page.read(cx);
            (page.web_search_pick.clone(), page.web_search_server.clone())
        });
        assert_eq!(pick.as_deref(), Some("mcp-2"));
        assert_eq!(server.as_deref(), Some("tinyfish"));
        assert!(harness.visual.debug_bounds("remove-web-search").is_some());

        // Switch away and back, then retool: the save names the entry.
        harness.pick("zhipu");
        assert_eq!(harness.active().as_deref(), Some("zhipu"));
        harness.pick("mcp-2");
        assert_eq!(harness.active().as_deref(), Some("mcp-2"));
        harness.click("web-search-tool");
        harness.choose("web-search-tool-option-1");
        harness.click("save-web-search");
        assert_eq!(
            harness.engine.saved.lock().unwrap().last(),
            Some(&serde_json::json!({
                "kind": "mcp",
                "id": "mcp-2",
                "server": "tinyfish",
                "tool": "web_search",
            }))
        );
        let entries = harness.engine.state.lock().unwrap().entries.clone();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].tool.as_deref(), Some("web_search"));
    }

    /// A reveal answered after the user picked another row is dropped:
    /// Zhipu's key must not land in the unsaved Brave draft.
    #[gpui::test]
    fn a_reveal_answered_after_a_new_pick_is_dropped(cx: &mut gpui::TestAppContext) {
        let harness = web_search_harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)], vec![]);
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.reveal_web_search_key(cx);
            page.pick_web_search_backend("brave".into(), cx);
        });
        harness.pump();
        assert_eq!(harness.key_text(), "");
        assert_eq!(harness.error(), None);
    }

    /// A server whose tools can't be listed swaps the Tool menu for a text
    /// field, so the entry can still be saved by name.
    #[gpui::test]
    fn an_unlistable_server_falls_back_to_typing_the_tool(cx: &mut gpui::TestAppContext) {
        let mut harness = web_search_harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)], vec![]);
        harness.pick(MCP_KIND);
        harness.click("web-search-server");
        harness.choose("web-search-server-option-1");
        harness.click("web-search-tool");
        assert!(harness.visual.debug_bounds("web-search-tool").is_none());
        assert!(
            harness
                .visual
                .debug_bounds("web-search-tool-input")
                .is_some()
        );
        harness.type_into(|page| &page.web_search_tool, "search");
        harness.click("save-web-search");
        assert_eq!(harness.error(), None);
        assert_eq!(
            harness.engine.saved.lock().unwrap().last(),
            Some(&serde_json::json!({ "kind": "mcp", "server": "broken", "tool": "search" }))
        );
    }

    /// Layout invariant: the in-field eye toggle stays inside the key field's
    /// box. The input's `w_full` root in a fixed-width flex row pushed the
    /// `flex_none` toggle out past the field's right border (user report).
    #[gpui::test]
    fn the_eye_toggle_stays_inside_the_key_field(cx: &mut gpui::TestAppContext) {
        let harness = web_search_harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)], vec![]);
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
            search_state(None, vec![]),
            &[],
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
            &[serde_json::json!({ "kind": "zhipu", "apiKey": "zhipu-key-123456" })]
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
        let mut harness = web_search_harness(cx, zhipu_active(), &[("zhipu", ZHIPU_KEY)], vec![]);
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
            search_state(None, vec![]),
            &[],
            vec![],
            false,
            JevSettingsState {
                api_key_masked: None,
            },
            None,
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

    /// The Jev group: the masked key from `GetJevSettings` is what the
    /// field shows; Save rides `SaveJevSettings` (an untouched field
    /// re-reads the stored key through `RevealJevKey` instead of writing
    /// the mask); the eye reveals and re-conceals; Remove clears the
    /// stored key.
    #[gpui::test]
    fn the_jev_group_masks_saves_reveals_and_removes(cx: &mut gpui::TestAppContext) {
        let harness = web_search_harness_with(
            cx,
            search_state(None, vec![]),
            &[],
            vec![],
            true,
            JevSettingsState {
                api_key_masked: Some("sk-j…mnop".into()),
            },
            Some("sk-jev-abcdefghijklmnop"),
        );

        // The masked display, never bullets; configured means no
        // unconfigured note.
        let text = |h: &WebSearchHarness| {
            h.visual
                .read(|cx| h.page.read(cx).jev_key.read(cx).text().to_string())
        };
        let stored = |h: &WebSearchHarness| {
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
            harness.engine.jev_saved.lock().unwrap().as_slice(),
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
