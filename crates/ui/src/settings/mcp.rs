//! The MCP Servers settings page (ADR-0034, ticket 08): device-level MCP
//! server definitions managed over the four settings RPCs — the list with
//! each server's transport and enabled state, add/edit and JSON-import
//! dialogs, the enabled toggle (effective from the next Turn), removal
//! behind a confirmation, and the Test probe surfacing status, tool
//! count, or the failure reason. The page renders RPC reply shapes only —
//! it never learns the MCP protocol exists.

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Task, Window, div,
    prelude::*, px,
};
use holt_rpc::methods;

use crate::{
    composer::ComposerInput,
    popover::{self, Loadable},
    settings::widgets,
    state::AppState,
    theme::{Theme, ink},
};

/// One server as the Get reply shapes it — the flat, hand-editable entry
/// form the engine serves.
#[derive(Debug, Clone, Default, PartialEq)]
struct McpServerView {
    name: String,
    enabled: bool,
    command: Option<String>,
    url: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    headers: BTreeMap<String, String>,
    bearer_token_env_var: Option<String>,
    enabled_tools: Vec<String>,
    disabled_tools: Vec<String>,
}

impl McpServerView {
    fn parse(value: &serde_json::Value) -> Self {
        let str_field = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let list_field = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let map_field = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_object)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|(name, value)| {
                            value
                                .as_str()
                                .map(|value| (name.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            name: str_field("name").unwrap_or_default(),
            enabled: value
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            command: str_field("command"),
            url: str_field("url"),
            args: list_field("args"),
            env: map_field("env"),
            cwd: str_field("cwd"),
            headers: map_field("headers"),
            bearer_token_env_var: str_field("bearerTokenEnvVar"),
            enabled_tools: list_field("enabledTools"),
            disabled_tools: list_field("disabledTools"),
        }
    }

    /// The flat Save payload — exactly the entry shape a hand edit or the
    /// engine's strict parse expects.
    fn to_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::Map::new();
        if let Some(command) = &self.command {
            payload.insert("command".into(), serde_json::json!(command));
        }
        if let Some(url) = &self.url {
            payload.insert("url".into(), serde_json::json!(url));
        }
        if !self.args.is_empty() {
            payload.insert("args".into(), serde_json::json!(self.args));
        }
        if !self.env.is_empty() {
            payload.insert("env".into(), serde_json::json!(self.env));
        }
        if let Some(cwd) = &self.cwd {
            payload.insert("cwd".into(), serde_json::json!(cwd));
        }
        if !self.headers.is_empty() {
            payload.insert("headers".into(), serde_json::json!(self.headers));
        }
        if let Some(var) = &self.bearer_token_env_var {
            payload.insert("bearerTokenEnvVar".into(), serde_json::json!(var));
        }
        if !self.enabled {
            payload.insert("enabled".into(), serde_json::json!(false));
        }
        if !self.enabled_tools.is_empty() {
            payload.insert("enabledTools".into(), serde_json::json!(self.enabled_tools));
        }
        if !self.disabled_tools.is_empty() {
            payload.insert(
                "disabledTools".into(),
                serde_json::json!(self.disabled_tools),
            );
        }
        serde_json::Value::Object(payload)
    }

    /// The row's transport summary: the stdio command line or the URL.
    fn transport_summary(&self) -> String {
        if let Some(command) = &self.command {
            if self.args.is_empty() {
                command.clone()
            } else {
                format!("{} {}", command, self.args.join(" "))
            }
        } else if let Some(url) = &self.url {
            url.clone()
        } else {
            "no transport".into()
        }
    }

    fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

/// Parse pasted JSON into importable entries (ADR-0034 ticket 08): the
/// Claude-style `{"mcpServers": {…}}` wrapper maps names from its keys; a
/// bare single-server object takes its name from a `"name"` field. Only
/// the essentials pre-validate here — the engine's strict parse is the
/// authority and its errors surface per entry.
fn parse_import(text: &str) -> Result<Vec<(String, McpServerView)>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|error| format!("not valid JSON: {error}"))?;
    let entries: Vec<(String, serde_json::Value)> = if let Some(map) = value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    {
        map.iter()
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect()
    } else if value
        .as_object()
        .is_some_and(|object| object.contains_key("command") || object.contains_key("url"))
    {
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                "a bare server object needs a \"name\" field — or wrap it in \
                     {\"mcpServers\": { … }}"
                    .to_string()
            })?;
        vec![(name.to_string(), value)]
    } else {
        return Err("expected a {\"mcpServers\": { … }} object".into());
    };
    if entries.is_empty() {
        return Err("no servers found in the JSON".into());
    }
    let mut parsed = Vec::with_capacity(entries.len());
    for (name, server) in entries {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "server name {name:?} is invalid: names use [A-Za-z0-9_-] only"
            ));
        }
        let mut view = McpServerView::parse(&server);
        view.name = name.clone();
        if view.command.is_none() && view.url.is_none() {
            return Err(format!(
                "server {name:?} needs either \"command\" (stdio) or \"url\" (http)"
            ));
        }
        parsed.push((name, view));
    }
    Ok(parsed)
}

/// What a probe reported, held per server name until the next probe.
#[derive(Debug, Clone)]
enum ProbeView {
    Running,
    Ok {
        tool_count: usize,
        tool_names: Vec<String>,
    },
    Failed {
        reason: String,
    },
}

/// Which transport the editor dialog is filling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorTransport {
    Stdio,
    Http,
}

/// Which input mode the ADD dialog is in: the structured form, or a
/// pasted `{"mcpServers": …}` JSON blob (import). Editing an existing
/// server is form-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    Form,
    Json,
}

/// The add/edit dialog. Plain fields ride `ComposerInput` entities; the
/// editing target (`None` = adding) names the server being replaced.
struct McpEditor {
    editing: Option<String>,
    mode: EditorMode,
    json: Entity<ComposerInput>,
    transport: EditorTransport,
    name: Entity<ComposerInput>,
    command: Entity<ComposerInput>,
    args: Entity<ComposerInput>,
    env: Entity<ComposerInput>,
    cwd: Entity<ComposerInput>,
    url: Entity<ComposerInput>,
    headers: Entity<ComposerInput>,
    bearer: Entity<ComposerInput>,
    enabled: bool,
    /// The last save's failure, shown inline until the form changes.
    error: Option<String>,
}

/// Parse `KEY=VALUE` lines — the env field's wire form. The value keeps
/// any `:` or later `=` it carries.
fn parse_env_lines(text: &str) -> BTreeMap<String, String> {
    parse_pair_lines(text, '=')
}

/// Parse `Header-Name: value` lines — the headers field's wire form. The
/// value keeps any `=` it carries.
fn parse_header_lines(text: &str) -> BTreeMap<String, String> {
    parse_pair_lines(text, ':')
}

fn parse_pair_lines(text: &str, separator: char) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let split = line
            .split_once(separator)
            .map(|(name, value)| (name.trim(), value.trim()))
            .filter(|(name, _)| !name.is_empty());
        if let Some((name, value)) = split {
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

pub struct McpPage {
    state: Entity<AppState>,
    servers: Loadable<Vec<McpServerView>>,
    validation_error: Option<String>,
    editor: Option<McpEditor>,
    probe: BTreeMap<String, ProbeView>,
    /// The server name awaiting remove confirmation.
    confirm_remove: Option<String>,
    /// One slot per in-flight RPC: a dropped gpui `Task` cancels its
    /// future, so concurrent actions (a probe plus an enabled toggle)
    /// must not evict each other.
    tasks: Vec<Task<()>>,
}

impl McpPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            servers: Loadable::Idle,
            validation_error: None,
            editor: None,
            probe: BTreeMap::new(),
            confirm_remove: None,
            tasks: Vec::new(),
        };
        page.load(cx);
        page
    }

    fn engine_call(
        &mut self,
        cx: &mut Context<Self>,
        method: &'static str,
        params: serde_json::Value,
        then: impl FnOnce(&mut Self, serde_json::Value) + 'static,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.servers = Loadable::Error("Engine not connected".into());
            return;
        };
        self.tasks.push(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => then(page, value),
                    Err(holt_rpc::RpcError::UnknownMethod(_)) => {
                        page.servers = Loadable::Error(
                            "MCP servers aren't available — the engine doesn't support \
                             them yet"
                                .into(),
                        );
                    }
                    Err(error) => {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.error = Some(error.to_string());
                        } else {
                            page.servers = Loadable::Error(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Fresh Get: definitions plus any file-level validation error.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.engine_call(
            cx,
            methods::GET_MCP_SETTINGS,
            serde_json::json!({}),
            |page, value| {
                page.apply_state(value);
            },
        );
    }

    fn save(&mut self, cx: &mut Context<Self>, name: String, server: McpServerView) {
        self.engine_call(
            cx,
            methods::SAVE_MCP_SERVER,
            serde_json::json!({ "name": name, "server": server.to_payload() }),
            |page, value| {
                page.editor = None;
                page.confirm_remove = None;
                page.apply_state(value);
            },
        );
    }

    fn remove(&mut self, cx: &mut Context<Self>, name: String) {
        self.engine_call(
            cx,
            methods::REMOVE_MCP_SERVER,
            serde_json::json!({ "name": name }),
            |page, value| {
                page.editor = None;
                page.confirm_remove = None;
                page.apply_state(value);
            },
        );
    }

    /// Import every parsed entry, one strict upsert each; the first
    /// failure keeps the dialog open with the reason (entries before it
    /// stay imported).
    fn import(&mut self, cx: &mut Context<Self>, entries: Vec<(String, McpServerView)>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.tasks.push(cx.spawn(async move |this, cx| {
            let client = engine.client();
            for (name, server) in entries {
                let result = client
                    .call(
                        methods::SAVE_MCP_SERVER,
                        serde_json::json!({ "name": name, "server": server.to_payload() }),
                    )
                    .await;
                if let Err(error) = result {
                    this.update(cx, |page, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.error = Some(format!("{name}: {error}"));
                        }
                        page.load(cx);
                    })
                    .ok();
                    return;
                }
            }
            this.update(cx, |page, cx| {
                page.editor = None;
                page.load(cx);
            })
            .ok();
        }));
    }

    fn probe(&mut self, cx: &mut Context<Self>, name: String) {
        self.probe.insert(name.clone(), ProbeView::Running);
        self.engine_call(
            cx,
            methods::TEST_MCP_SERVER,
            serde_json::json!({ "name": name }),
            move |page, value| {
                let view = if value["status"] == "ok" {
                    ProbeView::Ok {
                        tool_count: value["toolCount"].as_u64().unwrap_or(0) as usize,
                        tool_names: value["toolNames"]
                            .as_array()
                            .map(|names| {
                                names
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }
                } else {
                    ProbeView::Failed {
                        reason: value["reason"]
                            .as_str()
                            .unwrap_or("the probe failed")
                            .to_string(),
                    }
                };
                page.probe.insert(name, view);
            },
        );
    }

    fn apply_state(&mut self, value: serde_json::Value) {
        self.servers = Loadable::Ready(
            value["servers"]
                .as_array()
                .map(|rows| rows.iter().map(McpServerView::parse).collect())
                .unwrap_or_default(),
        );
        self.validation_error = value["validationError"].as_str().map(str::to_string);
        // Probe results for vanished servers go with them.
        let live: Vec<&String> = match &self.servers {
            Loadable::Ready(servers) => servers.iter().map(|server| &server.name).collect(),
            _ => Vec::new(),
        };
        self.probe.retain(|name, _| live.contains(&name));
    }

    fn open_editor(&mut self, server: Option<&McpServerView>, cx: &mut Context<Self>) {
        let transport = match server {
            Some(server) if server.is_http() => EditorTransport::Http,
            _ => EditorTransport::Stdio,
        };
        let editor = McpEditor {
            editing: server.map(|server| server.name.clone()),
            mode: EditorMode::Form,
            json: cx.new(|cx| ComposerInput::new("{\n  \"mcpServers\": { … }\n}", cx)),
            transport,
            name: cx.new(|cx| {
                let mut input = ComposerInput::new("name — tools become mcp__<name>__tool", cx);
                input.set_text(
                    server.map(|server| server.name.clone()).unwrap_or_default(),
                    cx,
                );
                input
            }),
            command: cx.new(|cx| {
                let mut input = ComposerInput::new("npx", cx);
                input.set_text(
                    server
                        .and_then(|server| server.command.clone())
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            args: cx.new(|cx| {
                let mut input = ComposerInput::new("one argument per line", cx);
                input.set_text(
                    server
                        .map(|server| server.args.join("\n"))
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            env: cx.new(|cx| {
                let mut input = ComposerInput::new("KEY=VALUE per line", cx);
                input.set_text(
                    server
                        .map(|server| {
                            server
                                .env
                                .iter()
                                .map(|(name, value)| format!("{name}={value}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            cwd: cx.new(|cx| {
                let mut input = ComposerInput::new("default: holt's own directory", cx);
                input.set_text(
                    server
                        .and_then(|server| server.cwd.clone())
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            url: cx.new(|cx| {
                let mut input = ComposerInput::new("https://example.com/mcp", cx);
                input.set_text(
                    server
                        .and_then(|server| server.url.clone())
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            headers: cx.new(|cx| {
                let mut input = ComposerInput::new("Name: value per line", cx);
                input.set_text(
                    server
                        .map(|server| {
                            server
                                .headers
                                .iter()
                                .map(|(name, value)| format!("{name}: {value}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            bearer: cx.new(|cx| {
                let mut input = ComposerInput::new("ENVIRONMENT_VARIABLE_NAME", cx);
                input.set_text(
                    server
                        .and_then(|server| server.bearer_token_env_var.clone())
                        .unwrap_or_default(),
                    cx,
                );
                input
            }),
            enabled: server.map(|server| server.enabled).unwrap_or(true),
            error: None,
        };
        self.editor = Some(editor);
        self.confirm_remove = None;
        cx.notify();
    }

    /// Build the Save payload from the form. Returns the name error first —
    /// the engine re-validates everything strictly anyway.
    fn editor_server(&self, cx: &App) -> Result<(String, McpServerView), String> {
        let editor = self.editor.as_ref().expect("editor open");
        let name = editor.name.read(cx).text().trim().to_string();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(
                "Names use [A-Za-z0-9_-] — the name rides every mcp__name__tool.".to_string(),
            );
        }
        let mut server = McpServerView {
            name: name.clone(),
            enabled: editor.enabled,
            ..Default::default()
        };
        match editor.transport {
            EditorTransport::Stdio => {
                let command = editor.command.read(cx).text().trim().to_string();
                if command.is_empty() {
                    return Err("A stdio server needs a command.".into());
                }
                server.command = Some(command);
                server.args = editor
                    .args
                    .read(cx)
                    .text()
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect();
                server.env = parse_env_lines(editor.env.read(cx).text());
                let cwd = editor.cwd.read(cx).text().trim().to_string();
                server.cwd = (!cwd.is_empty()).then_some(cwd);
            }
            EditorTransport::Http => {
                let url = editor.url.read(cx).text().trim().to_string();
                if url.is_empty() {
                    return Err("An http server needs a URL.".into());
                }
                server.url = Some(url);
                server.headers = parse_header_lines(editor.headers.read(cx).text());
                let bearer = editor.bearer.read(cx).text().trim().to_string();
                server.bearer_token_env_var = (!bearer.is_empty()).then_some(bearer);
            }
        }
        Ok((name, server))
    }

    /// The page's top action row — right-aligned, content-hugging.
    fn render_actions(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let add_theme = theme.clone();
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .id("mcp-open-add")
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(move |style| widgets::ghost_hover(&add_theme, style))
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.open_editor(None, cx);
                    }))
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
                            .child("Add Server"),
                    ),
            )
            .into_any_element()
    }

    fn render_server_row(
        &mut self,
        theme: &Theme,
        server: &McpServerView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = server.name.clone();
        let toggle_theme = theme.clone();
        let toggle_server = server.clone();
        let toggle_name = name.clone();
        // The row: name + transport summary, the enabled toggle, and three
        // quiet text actions.
        let mut row = div()
            .w_full()
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(8.0))
            .hover(|state| state.bg(ink(0.03)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(widgets::ROW_TITLE_SIZE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if server.enabled {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(server.name.clone())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{} · {}",
                                if server.is_http() { "http" } else { "stdio" },
                                server.transport_summary()
                            ))),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("mcp-toggle-{name}")))
                    .flex_none()
                    .cursor_pointer()
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let mut server = toggle_server.clone();
                        server.enabled = !server.enabled;
                        page.save(cx, toggle_name.clone(), server);
                    }))
                    .child(widgets::toggle_switch(&toggle_theme, server.enabled)),
            );
        for label in ["Test", "Edit", "Remove"] {
            let action_theme = theme.clone();
            let action_name = name.clone();
            let danger = label == "Remove";
            row = row.child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!(
                        "mcp-{}-{name}",
                        label.to_lowercase()
                    )))
                    .text_color(if danger {
                        theme.danger_muted
                    } else {
                        theme.text_muted
                    })
                    .hover(move |style| widgets::ghost_hover(&action_theme, style))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        match label {
                            "Test" => page.probe(cx, action_name.clone()),
                            "Edit" => {
                                let server = page.servers.ready().and_then(|servers| {
                                    servers
                                        .iter()
                                        .find(|server| server.name == action_name)
                                        .cloned()
                                });
                                page.open_editor(server.as_ref(), cx);
                            }
                            _ => {
                                page.confirm_remove = Some(action_name.clone());
                                page.editor = None;
                                cx.notify();
                            }
                        }
                    }))
                    .child(label),
            );
        }
        let mut column = div().flex().flex_col().gap(px(6.0)).child(row);
        // The remove confirmation — one short question, two quiet answers.
        if self.confirm_remove.as_deref() == Some(server.name.as_str()) {
            let confirm_theme = theme.clone();
            let confirm_name = name.clone();
            let cancel_theme = theme.clone();
            column = column.child(
                div()
                    .id(SharedString::from(format!("mcp-remove-confirm-{name}")))
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("Remove {}?", server.name))),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(SharedString::from(format!("mcp-remove-yes-{name}")))
                            .text_color(theme.danger)
                            .hover(move |style| widgets::ghost_hover(&confirm_theme, style))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.remove(cx, confirm_name.clone());
                            }))
                            .child("Remove"),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(SharedString::from(format!("mcp-remove-no-{name}")))
                            .hover(move |style| widgets::ghost_hover(&cancel_theme, style))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.confirm_remove = None;
                                cx.notify();
                            }))
                            .child("Cancel"),
                    ),
            );
        }
        // The probe result.
        if let Some(view) = self.probe.get(&server.name) {
            let strip = match view {
                ProbeView::Running => widgets::row_description(theme, "Testing…"),
                ProbeView::Ok {
                    tool_count,
                    tool_names,
                } => {
                    let names = if tool_names.len() > 8 {
                        format!("{} …", tool_names[..8].join(", "))
                    } else {
                        tool_names.join(", ")
                    };
                    widgets::row_description(theme, format!("ok · {tool_count} tools: {names}"))
                }
                ProbeView::Failed { reason } => {
                    widgets::error_strip(theme, format!("Test failed: {reason}"))
                }
            };
            column = column.child(strip);
        }
        column.into_any_element()
    }

    /// One labeled dialog field: label, then the bordered input.
    fn dialog_field(theme: &Theme, label: &str, input: &Entity<ComposerInput>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(widgets::field_label(theme, label))
            .child(
                div()
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.input_glass_bg())
                    .child(input.clone()),
            )
    }

    fn render_editor_dialog(
        &mut self,
        theme: &Theme,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let editing = editor.editing.clone();
        let adding = editing.is_none();
        let transport = editor.transport;
        let json_mode = adding && editor.mode == EditorMode::Json;
        let mut form = div().mt(px(12.0)).flex().flex_col().gap(px(12.0));
        // The mode picker — adding only: the structured form, or a pasted
        // mcpServers JSON blob. Editing is form-only.
        if adding {
            let mut modes = div().flex().flex_row().gap(px(6.0));
            for (option, label) in [(EditorMode::Form, "Form"), (EditorMode::Json, "JSON")] {
                let selected = if json_mode {
                    option == EditorMode::Json
                } else {
                    option == EditorMode::Form
                };
                let button_theme = theme.clone();
                modes = modes.child(
                    widgets::ghost_action(theme)
                        .id(SharedString::from(format!(
                            "mcp-mode-{}",
                            if option == EditorMode::Form {
                                "form"
                            } else {
                                "json"
                            }
                        )))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(if selected {
                            theme.border_strong
                        } else {
                            theme.border
                        })
                        .when(selected, |button| {
                            button.bg(ink(0.06)).text_color(theme.text)
                        })
                        .hover(move |style| widgets::ghost_hover(&button_theme, style))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            if let Some(editor) = page.editor.as_mut() {
                                editor.mode = option;
                                editor.error = None;
                            }
                            cx.notify();
                        }))
                        .child(label),
                );
            }
            form = form.child(modes);
        }
        if json_mode {
            form = form.child(
                div()
                    .px(px(10.0))
                    .py(px(7.0))
                    .min_h(px(160.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.input_glass_bg())
                    .child(editor.json.clone()),
            );
        } else {
            if adding {
                form = form.child(Self::dialog_field(theme, "Name", &editor.name));
            }
            // The transport picker: two segmented buttons.
            let mut transport_row = div().flex().flex_row().gap(px(6.0));
            for (option, label) in [
                (EditorTransport::Stdio, "Local (stdio)"),
                (EditorTransport::Http, "Remote (http)"),
            ] {
                let selected = transport == option;
                let button_theme = theme.clone();
                transport_row = transport_row.child(
                    widgets::ghost_action(theme)
                        .id(SharedString::from(format!(
                            "mcp-transport-{}",
                            if option == EditorTransport::Stdio {
                                "stdio"
                            } else {
                                "http"
                            }
                        )))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(if selected {
                            theme.border_strong
                        } else {
                            theme.border
                        })
                        .when(selected, |button| {
                            button.bg(ink(0.06)).text_color(theme.text)
                        })
                        .hover(move |style| widgets::ghost_hover(&button_theme, style))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            if let Some(editor) = page.editor.as_mut() {
                                editor.transport = option;
                                editor.error = None;
                            }
                            cx.notify();
                        }))
                        .child(label),
                );
            }
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(widgets::field_label(theme, "Transport"))
                    .child(transport_row),
            );
            match transport {
                EditorTransport::Stdio => {
                    form = form
                        .child(Self::dialog_field(theme, "Command", &editor.command))
                        .child(Self::dialog_field(theme, "Arguments", &editor.args))
                        .child(Self::dialog_field(theme, "Environment", &editor.env))
                        .child(Self::dialog_field(theme, "Working directory", &editor.cwd));
                }
                EditorTransport::Http => {
                    form = form
                        .child(Self::dialog_field(theme, "URL", &editor.url))
                        .child(Self::dialog_field(theme, "Headers", &editor.headers))
                        .child(Self::dialog_field(
                            theme,
                            "Bearer token env var",
                            &editor.bearer,
                        ));
                }
            }
            // Enabled toggle.
            let enabled = editor.enabled;
            form = form.child(
                div()
                    .id("mcp-editor-enabled")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.enabled = !enabled;
                        }
                        cx.notify();
                    }))
                    .child(widgets::toggle_switch(theme, editor.enabled))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child("Enabled"),
                    ),
            );
        }
        if let Some(error) = &editor.error {
            form = form.child(widgets::error_strip(theme, error.clone()));
        }
        let cancel_theme = theme.clone();
        let card = popover::dialog_card(theme)
            .w(px(440.0))
            .id("mcp-editor-card")
            // A mask click dismisses — the same outside-click close the
            // popover menus use.
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.editor = None;
                cx.notify();
            }))
            .on_key_down(cx.listener(|page, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    page.editor = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(
                theme,
                &match &editing {
                    Some(name) => format!("Edit {name}"),
                    None => "Add MCP server".to_string(),
                },
            ))
            .when(adding, |card| {
                card.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.text_muted)
                        .child(
                            "Tools become mcp__<name>__tool in every chat, behind \
                             the same permission gate.",
                        ),
                )
            })
            .child(form)
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "mcp-editor-cancel")
                            .id("mcp-editor-cancel")
                            .hover(move |style| widgets::ghost_hover(&cancel_theme, style))
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.editor = None;
                                cx.notify();
                            })),
                    )
                    .child(if json_mode {
                        popover::btn_primary(theme, "Import")
                            .id("mcp-editor-import")
                            .on_click(cx.listener(|page, _, _, cx| {
                                let text = page
                                    .editor
                                    .as_ref()
                                    .map(|editor| editor.json.read(cx).text().to_string())
                                    .unwrap_or_default();
                                match parse_import(&text) {
                                    Ok(entries) => page.import(cx, entries),
                                    Err(error) => {
                                        if let Some(editor) = page.editor.as_mut() {
                                            editor.error = Some(error);
                                        }
                                        cx.notify();
                                    }
                                }
                            }))
                    } else {
                        popover::btn_primary(theme, "Save")
                            .id("mcp-editor-save")
                            .on_click(cx.listener(|page, _, _, cx| match page.editor_server(cx) {
                                Ok((name, server)) => page.save(cx, name, server),
                                Err(error) => {
                                    if let Some(editor) = page.editor.as_mut() {
                                        editor.error = Some(error);
                                    }
                                    cx.notify();
                                }
                            }))
                    }),
            )
            .into_any_element();
        popover::modal("mcp-editor-dialog", viewport, card)
    }
}

impl Render for McpPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body = match &self.servers {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("mcp-skeleton", &theme, 3, cx.entity_id(), cx)
                    .into_any_element()
            }
            Loadable::Error(error) => {
                widgets::error_strip(&theme, error.clone()).into_any_element()
            }
            Loadable::Ready(servers) => {
                let servers = servers.clone();
                let mut column = div().mt(px(14.0)).flex().flex_col().gap(px(2.0));
                if servers.is_empty() {
                    column = column.child(
                        div()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text_muted)
                            .child("No servers configured."),
                    );
                }
                for server in servers {
                    column = column.child(self.render_server_row(&theme, &server, cx));
                }
                column.into_any_element()
            }
        };
        let mut page = widgets::page_column()
            .child(widgets::page_header(
                &theme,
                "MCP Servers",
                self.servers.ready().map(Vec::len),
            ))
            .child(widgets::page_subtitle(
                &theme,
                "External tool providers; their tools join every chat behind the \
                 same permission gate.",
            ))
            .child(self.render_actions(&theme, cx));
        if let Some(error) = self.validation_error.clone() {
            page = page.child(widgets::warning_strip(
                &theme,
                format!("mcp.json is invalid and was not applied: {error}"),
            ));
        }
        page = page.child(body);
        // The dialogs ride the deferred layer above everything.
        let viewport = window.viewport_size();
        if self.editor.is_some() {
            let dialog = self.render_editor_dialog(&theme, viewport, cx);
            return div().child(page).child(dialog).into_any_element();
        }
        page.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(value: serde_json::Value) -> McpServerView {
        McpServerView::parse(&value)
    }

    #[test]
    fn views_parse_the_flat_reply_shape() {
        let view = server(serde_json::json!({
            "name": "example",
            "command": "npx",
            "args": ["-y", "server"],
            "env": { "KEY": "value" },
            "cwd": "/tmp",
            "enabled": false,
            "enabledTools": ["echo"],
            "disabledTools": ["noisy"],
        }));
        assert_eq!(view.name, "example");
        assert!(!view.enabled);
        assert_eq!(view.command.as_deref(), Some("npx"));
        assert_eq!(view.args, vec!["-y", "server"]);
        assert_eq!(view.env.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(view.cwd.as_deref(), Some("/tmp"));
        assert_eq!(view.enabled_tools, vec!["echo"]);
        assert_eq!(view.disabled_tools, vec!["noisy"]);
        assert_eq!(view.transport_summary(), "npx -y server");
        assert!(!view.is_http());

        let view = server(serde_json::json!({
            "name": "remote",
            "url": "https://example.com/mcp",
            "bearerTokenEnvVar": "TOKEN_VAR",
            "enabled": true,
        }));
        assert!(view.is_http());
        assert_eq!(view.transport_summary(), "https://example.com/mcp");
        assert_eq!(view.bearer_token_env_var.as_deref(), Some("TOKEN_VAR"));
    }

    #[test]
    fn payloads_round_trip_through_the_flat_shape() {
        let view = server(serde_json::json!({
            "name": "example",
            "command": "npx",
            "args": ["-y", "server"],
            "enabled": false,
        }));
        let payload = view.to_payload();
        assert_eq!(payload["command"], "npx");
        assert_eq!(payload["enabled"], false);
        // Defaults stay off the wire.
        assert!(payload.get("cwd").is_none());
        assert!(payload.get("enabledTools").is_none());
    }

    #[test]
    fn env_lines_split_on_equals_only() {
        let map = parse_env_lines("A=1\nKEY=http://x/y\n# not a pair\n\nB=x=y");
        assert_eq!(map.get("A").map(String::as_str), Some("1"));
        // A URL in the value keeps its colons and slashes.
        assert_eq!(map.get("KEY").map(String::as_str), Some("http://x/y"));
        assert_eq!(map.get("B").map(String::as_str), Some("x=y"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn header_lines_split_on_colons_only() {
        let map = parse_header_lines("Authorization: Bearer abc=def\nX-Static: 1\nbroken");
        // An `=` inside the value never splits the header.
        assert_eq!(
            map.get("Authorization").map(String::as_str),
            Some("Bearer abc=def")
        );
        assert_eq!(map.get("X-Static").map(String::as_str), Some("1"));
        // A line without the separator is dropped, never half-parsed.
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn import_parses_the_claude_style_wrapper() {
        let entries = parse_import(
            r#"{
  "mcpServers": {
    "dashboard-icons": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "https://dashboardicons.com/api/mcp"]
    }
  }
}"#,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        let (name, view) = &entries[0];
        assert_eq!(name, "dashboard-icons");
        assert_eq!(view.command.as_deref(), Some("npx"));
        assert_eq!(
            view.args,
            vec!["-y", "mcp-remote", "https://dashboardicons.com/api/mcp"]
        );
        // The payload carries no name field — the engine rejects one.
        assert!(view.to_payload().get("name").is_none());
    }

    #[test]
    fn import_parses_multiple_servers_and_bare_entries() {
        let entries = parse_import(
            r#"{"mcpServers": {
                "a": { "command": "x" },
                "b": { "url": "https://b/mcp" }
            }}"#,
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "a");
        assert_eq!(entries[1].0, "b");
        // A bare single-server object needs an explicit name.
        let (name, view) = parse_import(r#"{ "name": "solo", "url": "https://solo/mcp" }"#)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(name, "solo");
        assert_eq!(view.url.as_deref(), Some("https://solo/mcp"));
    }

    #[test]
    fn import_rejects_with_the_reason_named() {
        // Broken JSON.
        assert!(
            parse_import("{broken")
                .unwrap_err()
                .contains("not valid JSON")
        );
        // The wrong shape.
        assert!(
            parse_import(r#"[1, 2]"#)
                .unwrap_err()
                .contains("mcpServers")
        );
        // A bare object without a name.
        assert!(
            parse_import(r#"{ "command": "x" }"#)
                .unwrap_err()
                .contains("\"name\"")
        );
        // An empty wrapper.
        assert!(
            parse_import(r#"{"mcpServers": {}}"#)
                .unwrap_err()
                .contains("no servers")
        );
        // An illegal server name.
        assert!(
            parse_import(r#"{"mcpServers": {"my server": {"command": "x"}}}"#)
                .unwrap_err()
                .contains("[A-Za-z0-9_-]")
        );
        // A transportless entry.
        assert!(
            parse_import(r#"{"mcpServers": {"x": {"enabled": true}}}"#)
                .unwrap_err()
                .contains("command")
        );
    }
}
