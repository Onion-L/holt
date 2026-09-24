//! The engine's MCP client layer (ADR-0034): an app-scoped, lazily-started
//! pool of MCP connections owned by the engine. Every tool a server lists
//! is wrapped as an ordinary agent tool named `mcp__<server>__<tool>` —
//! from the model's and the transcript's perspective an MCP tool is just a
//! tool. Transports: stdio children today, Streamable HTTP in ticket 04.
//!
//! [`McpPool::agent_tools`] is the Turn-start snapshot: enabled servers
//! connect on demand, list their tools once, and the wrapped set is frozen
//! into the running Turn. A server that fails to start or answer is
//! skipped for that Turn with a log line and retried the same lazy way on
//! the next one — no background restart loop.
//!
//! The model-facing surface defers the schemas (ADR-0036): main-chat
//! Turns also mount a small `mcp_tools` loader and append a synthetic
//! call/result pair of it to every LLM request, whose `added_tool_names`
//! marks every mounted MCP tool — providers with deferred-tool support
//! keep the schemas out of the static context until the model loads and
//! calls them.

pub(crate) mod config;

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use futures::future::BoxFuture;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{
        AssistantContent, AssistantMessage, BlockContent, Message, Model, TextContent, ToolCall,
        ToolResultMessage,
    },
};
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, PaginatedRequestParams, Tool,
    },
    service::{RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use config::{McpServer, McpStore, ServerTransport};

/// Expand one config value at run time, logging any variable that stayed
/// literal (ADR-0034: a missing variable is visible, never silently
/// empty).
fn expand(value: &str) -> String {
    let (expanded, missing) = config::expand_env(value);
    if !missing.is_empty() {
        tracing::warn!(
            target: "holt::mcp",
            missing = %missing.join(", "),
            value = %value,
            "mcp config references unset environment variables; kept literally"
        );
    }
    expanded
}

/// The app-scoped connection pool. One instance lives on the runtime for
/// the whole engine; connections start lazily (nothing spawns at app
/// startup) and are shared by every chat on the device.
pub(crate) struct McpPool {
    servers: McpStore,
    connections: tokio::sync::Mutex<HashMap<String, Arc<LiveServer>>>,
}

impl McpPool {
    pub(crate) fn load(data_dir: &Path) -> Result<Self, EngineError> {
        Ok(Self {
            servers: McpStore::load(data_dir)?,
            connections: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The Turn-start snapshot (ADR-0034): every enabled server's listed
    /// tools, wrapped as standard agent tools. Called once per Turn
    /// admission by main-chat runs. The file is refreshed first — a hand
    /// edit while holt runs lands from the next Turn; a broken one keeps
    /// the last-good set (logged).
    pub(crate) async fn agent_tools(&self) -> Vec<AgentTool> {
        if let Err(error) = self.servers.refresh() {
            tracing::warn!(
                target: "holt::mcp",
                %error,
                "mcp config failed to reload; keeping the last-good servers"
            );
        }
        let mut tools = Vec::new();
        let mut connections = self.connections.lock().await;
        let definitions = self.servers.get();
        // Connections for servers that vanished from the config or were
        // disabled die now — dropping the pool's Arc shuts the child down
        // (an Arc a running Turn still holds keeps it alive until that
        // Turn ends, so a mid-Turn disable never kills a tool in use).
        connections.retain(|name, _| definitions.get(name).is_some_and(|server| server.enabled));
        for (name, server) in definitions {
            if !server.enabled {
                continue;
            }
            // A cached connection whose list has gone stale (a child that
            // died since the last Turn) is dropped and reconnected in the
            // same pass — a server restarted between Turns works again on
            // the very next one, no app restart (ADR-0034).
            let mut fresh: Option<(Arc<LiveServer>, Vec<Tool>)> = None;
            if let Some(connection) = connections.get(&name).cloned() {
                match connection.list_tools().await {
                    Ok(list) => fresh = Some((connection, list)),
                    Err(error) => {
                        tracing::warn!(
                            target: "holt::mcp",
                            server = %name,
                            %error,
                            "mcp connection went stale; reconnecting for this turn"
                        );
                        connections.remove(&name);
                    }
                }
            }
            let hit = match fresh {
                Some(hit) => Some(hit),
                None => connect_fresh(&mut connections, &name, &server).await,
            };
            let Some((connection, list)) = hit else {
                continue;
            };
            tools.extend(
                list.into_iter()
                    .filter(|tool| server.allows_tool(&tool.name))
                    .map(|tool| wrap_server_tool(&name, tool, &connection)),
            );
        }
        tools
    }

    /// One on-demand probe (the Settings Test action): connect, list
    /// tools, disconnect — never touching the pool's live connections,
    /// never standing watch (ADR-0034).
    pub(crate) async fn probe(&self, name: &str) -> ProbeReport {
        // Fresh definitions: a hand edit since the last Turn is testable
        // immediately, and a broken file reports its own error.
        if let Err(reason) = self.servers.refresh() {
            return ProbeReport::Failed { reason };
        }
        let server = self.servers.get().get(name).cloned();
        let Some(server) = server else {
            return ProbeReport::Failed {
                reason: format!("unknown mcp server {name:?}"),
            };
        };
        let connection = match connect(&server).await {
            Ok(connection) => connection,
            Err(reason) => return ProbeReport::Failed { reason },
        };
        // The listing rides the same budget as the handshake: a server
        // that answers initialize but hangs on tools/list must fail the
        // probe, not park the Settings row on "Testing…" forever.
        let startup = Duration::from_millis(server.startup_timeout_ms);
        let tools = match tokio::time::timeout(startup, connection.list_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(error)) => {
                connection.shutdown().await;
                return ProbeReport::Failed { reason: error };
            }
            Err(_) => {
                connection.shutdown().await;
                return ProbeReport::Failed {
                    reason: "listing tools exceeded the startup timeout".into(),
                };
            }
        };
        connection.shutdown().await;
        // The same refusal a Turn's mount applies: an illegal or overlong
        // name would make the probe report a server the engine will not
        // mount.
        if let Some(bad) = tools.iter().find(|tool| !tool_name_is_legal(&tool.name)) {
            return ProbeReport::Failed {
                reason: format!(
                    "lists an illegal tool name {:?}; the server is refused at connect",
                    bad.name
                ),
            };
        }
        ProbeReport::Ok {
            tool_count: tools.len(),
            tool_names: tools
                .into_iter()
                .map(|tool| tool.name.to_string())
                .collect(),
        }
    }

    /// Drop one server's cached connection (the upsert/remove paths): the
    /// next Turn reconnects — or not — under the new definition.
    pub(crate) async fn invalidate(&self, name: &str) {
        self.connections.lock().await.remove(name);
    }

    /// The last-good definitions (the Settings view's source).
    pub(crate) fn definitions(&self) -> BTreeMap<String, McpServer> {
        self.servers.get()
    }

    /// The store behind the pool — the Settings quartet persists through
    /// it.
    pub(crate) fn store(&self) -> &McpStore {
        &self.servers
    }

    /// Re-read the file, returning any validation error (the Settings
    /// page's file-level feedback) while the pool keeps the last-good
    /// set.
    pub(crate) fn refresh_error(&self) -> Option<String> {
        self.servers.refresh().err()
    }
}

/// Connect (or reconnect) one server and list its tools, caching the
/// live connection on success. `None` — with a log line — skips the
/// server for this Turn.
async fn connect_fresh(
    connections: &mut HashMap<String, Arc<LiveServer>>,
    name: &str,
    server: &McpServer,
) -> Option<(Arc<LiveServer>, Vec<Tool>)> {
    match connect(server).await {
        Ok(connection) => {
            let connection = Arc::new(connection);
            let list = match connection.list_tools().await {
                Ok(list) => list,
                Err(error) => {
                    tracing::warn!(
                        target: "holt::mcp",
                        server = %name,
                        %error,
                        "mcp server failed to list tools; skipping it for this turn"
                    );
                    return None;
                }
            };
            // A server listing an illegal or overlong tool name is
            // rejected here — truncating would make an approval rule
            // point at the wrong tool (ADR-0034).
            if let Some(bad) = list.iter().find(|tool| !tool_name_is_legal(&tool.name)) {
                tracing::warn!(
                    target: "holt::mcp",
                    server = %name,
                    tool = %bad.name,
                    "mcp server lists an illegal tool name; refusing the server"
                );
                return None;
            }
            connections.insert(name.to_string(), Arc::clone(&connection));
            Some((connection, list))
        }
        // A server that cannot start is skipped for this Turn (log line,
        // tools absent); the next Turn retries the same lazy way.
        Err(error) => {
            tracing::warn!(
                target: "holt::mcp",
                server = %name,
                %error,
                "mcp server failed to connect; skipping it for this turn"
            );
            None
        }
    }
}

/// One live server connection: the running client service over its
/// transport. Holding it keeps the child (or HTTP session) alive; dropping
/// the last reference shuts it down. The per-call timeout (the server's
/// `toolTimeoutMs`, default 60 s) is every `tools/call`'s hard wall — a
/// hung server cannot wedge the chat.
struct LiveServer {
    service: RunningService<RoleClient, ()>,
    tool_timeout: Duration,
}

impl LiveServer {
    /// `tools/list`, following pagination to completion with a 100-page
    /// cap (ADR-0034) — a server that pages past the ceiling is refused,
    /// never silently truncated.
    async fn list_tools(&self) -> Result<Vec<Tool>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self
                .service
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(|error| service_error(&error))?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(tools),
            }
        }
        Err(format!(
            "tools/list paged past the {MAX_LIST_PAGES}-page ceiling"
        ))
    }

    /// One `tools/call`.
    async fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<CallToolResult, String> {
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = arguments.as_object().cloned();
        let response = tokio::time::timeout(self.tool_timeout, self.service.call_tool_once(params))
            .await
            .map_err(|_| {
                format!("mcp tool {name:?} exceeded its call timeout; the call was aborted")
            })?
            .map_err(|error| service_error(&error))?;
        match response {
            CallToolResponse::Complete(result) => Ok(result),
            // An MRTR `input_required` round cannot be answered by the
            // loop — settle it as the readable error it is.
            CallToolResponse::InputRequired(_) => Err(format!(
                "mcp tool {name:?} asked for interactive input, which holt does not serve"
            )),
            CallToolResponse::Task(_) => Err(format!(
                "mcp tool {name:?} returned an async task, which holt does not poll"
            )),
            _ => Err(format!(
                "mcp tool {name:?} returned an unsupported result shape"
            )),
        }
    }

    /// Cancel the service and wait for its transport to wind down — the
    /// probe path's clean disconnect.
    async fn shutdown(self) {
        let cancel = self.service.cancellation_token();
        cancel.cancel();
        let _ = self.service.waiting().await;
    }
}

fn service_error(error: &ServiceError) -> String {
    format!("mcp server error: {error}")
}

/// Connect one server per its definition. Config values expand here —
/// run time, never stored — and a stdio child inherits a sanitized
/// environment (credential-shaped variables stripped unless the server's
/// own `env` sets them, ADR-0034). The startup timeout bounds the
/// initialize handshake.
async fn connect(server: &McpServer) -> Result<LiveServer, String> {
    let timeout = Duration::from_millis(server.startup_timeout_ms);
    let tool_timeout = Duration::from_millis(server.tool_timeout_ms);
    match &server.transport {
        ServerTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let command = expand(command);
            let args = args.iter().map(|arg| expand(arg)).collect::<Vec<_>>();
            let env = env
                .iter()
                .map(|(name, value)| (name.clone(), expand(value)))
                .collect::<BTreeMap<_, _>>();
            // The child's working directory is the server's own `cwd` or
            // Holt's process cwd — never the chat's working directory
            // (ADR-0034): server behavior must not change with whichever
            // chat runs first.
            let cwd = cwd.as_deref().map(expand);
            let sanitized = config::sanitize_child_env(&env);
            let mut process = tokio::process::Command::new(&command);
            process.args(&args).env_clear().envs(&sanitized);
            if let Some(cwd) = cwd {
                process.current_dir(cwd);
            }
            let transport = TokioChildProcess::new(process)
                .map_err(|error| format!("could not spawn {command:?}: {error}"))?;
            let service = tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| format!("{command:?} did not initialize within its startup timeout"))?
                .map_err(|error| format!("could not initialize {command:?}: {error}"))?;
            Ok(LiveServer {
                service,
                tool_timeout,
            })
        }
        ServerTransport::Http {
            url,
            headers,
            bearer_token_env_var,
        } => {
            let url = expand(url);
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| format!("header name {name:?} is invalid: {error}"))?;
                let value = reqwest::header::HeaderValue::from_str(&expand(value))
                    .map_err(|error| format!("header {name:?} value is invalid: {error}"))?;
                config.custom_headers.insert(name, value);
            }
            if let Some(var) = bearer_token_env_var {
                // Read from the named environment variable here — the value
                // never lands in the config file, a log line, or an error
                // string (ADR-0034).
                let token = std::env::var(var)
                    .map_err(|_| format!("bearer token environment variable {var:?} is not set"))?;
                config.auth_header = Some(token);
            }
            let transport =
                StreamableHttpClientTransport::with_client(reqwest_mcp::Client::new(), config);
            let service = tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| format!("{url:?} did not initialize within its startup timeout"))?
                .map_err(|error| format!("could not initialize {url:?}: {error}"))?;
            Ok(LiveServer {
                service,
                tool_timeout,
            })
        }
    }
}

/// Resource ceilings (ADR-0034): what an MCP server may push into the
/// model's context before the engine caps it.
/// Tool descriptions truncate at 2 KB.
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
/// Result text caps at 100k characters, joined text blocks included.
const MAX_RESULT_CHARS: usize = 100_000;
/// `tools/list` pagination follows to completion, but stops — and refuses
/// the listing — past this many pages.
const MAX_LIST_PAGES: usize = 100;
/// A tool name the two-level scheme can carry verbatim (the 2025-11-25
/// spec's `[a-zA-Z0-9_-]{1,64}`). A server listing anything else is
/// rejected at connect; names are never truncated or rewritten.
fn tool_name_is_legal(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Truncate a tool description at the 2 KB ceiling with an in-band marker.
fn truncate_description(description: &str) -> String {
    if description.len() <= MAX_DESCRIPTION_BYTES {
        return description.to_string();
    }
    // Cut on a char boundary at or before the cap, keeping room for the
    // marker.
    let mut end = MAX_DESCRIPTION_BYTES;
    while end > 0 && !description.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n…[description truncated at {} bytes]",
        &description[..end],
        MAX_DESCRIPTION_BYTES
    )
}

/// Cap joined result text at the 100k-character ceiling with an in-band
/// marker.
fn truncate_result(text: &str) -> String {
    if text.chars().count() <= MAX_RESULT_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_RESULT_CHARS).collect();
    out.push_str(&format!(
        "\n…[result truncated at {} characters]",
        MAX_RESULT_CHARS
    ));
    out
}

/// Wrap one listed server tool as a standard agent tool (ADR-0034): the
/// two-level name, the description (capped at 2 KB) and input schema as
/// received, execute forwarding to `tools/call` under the server's call
/// timeout. Text content blocks join with newlines, cap at 100k
/// characters, and drop non-text blocks with an in-band notice;
/// `isError` results settle as error results the model reads.
fn wrap_server_tool(server: &str, tool: Tool, connection: &Arc<LiveServer>) -> AgentTool {
    let tool_name = tool.name.to_string();
    let label = tool
        .title
        .clone()
        .unwrap_or_else(|| format!("MCP {server} · {tool_name}"));
    let description = truncate_description(tool.description.as_deref().unwrap_or_default());
    let parameters = serde_json::Value::Object((*tool.input_schema).clone());
    let connection = Arc::clone(connection);
    AgentTool {
        name: format!("mcp__{server}__{tool_name}"),
        label,
        description,
        parameters,
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let connection = Arc::clone(&connection);
                let name = tool_name.clone();
                let params = params.clone();
                Box::pin(async move {
                    let result = connection.call(&name, &params).await?;
                    // Join, cap, then append the non-text notice — the
                    // notice must survive the cap, or a flood would hide
                    // that content was dropped at all.
                    let (joined, dropped_non_text) = split_text_content(&result);
                    let mut text = truncate_result(&joined);
                    if dropped_non_text {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str("[non-text content blocks were dropped]");
                    }
                    if result.is_error.unwrap_or(false) {
                        let reason = if text.trim().is_empty() {
                            format!("mcp tool {name:?} reported an error without detail")
                        } else {
                            text
                        };
                        return Err(reason);
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text,
                            ..Default::default()
                        })],
                        ..Default::default()
                    })
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// Split a result's content: text blocks joined with newlines, plus
/// whether any non-text block (images, audio, embedded resources) was
/// dropped. `structuredContent` is ignored (ADR-0034).
fn split_text_content(result: &CallToolResult) -> (String, bool) {
    let mut parts = Vec::new();
    let mut dropped_non_text = false;
    for block in &result.content {
        match block {
            rmcp::model::ContentBlock::Text(text) => parts.push(text.text.clone()),
            _ => dropped_non_text = true,
        }
    }
    (parts.join("\n"), dropped_non_text)
}

/// The discovery surface for deferred MCP tools (ADR-0036): one small
/// always-immediate tool. It reads only the Turn's own snapshot — never
/// the gate's mutating predicate (`mcp__` prefix misses `mcp_tools`), so
/// calls pass straight through.
pub(crate) const LOADER_TOOL_NAME: &str = "mcp_tools";

/// Loading is deliberately batch-small: every definition returns the full
/// description and schema, and a flood defeats the deferral.
const MAX_LOAD_PER_CALL: usize = 8;

/// A catalog line carries the description's first line, capped.
const CATALOG_DESCRIPTION_CHARS: usize = 120;

/// One mounted tool as the loader sees it: the definition it hands out
/// and the name it marks.
#[derive(Clone)]
struct MountedTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

fn mounted(tools: &[AgentTool]) -> Vec<MountedTool> {
    tools
        .iter()
        .map(|tool| MountedTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

/// The catalog's one-line description: first line, whitespace collapsed,
/// capped with an in-band marker.
fn one_line(description: &str) -> String {
    let first = description
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first.chars().count() <= CATALOG_DESCRIPTION_CHARS {
        return first;
    }
    let cut: String = first.chars().take(CATALOG_DESCRIPTION_CHARS).collect();
    format!("{cut}…")
}

/// The catalog text the bootstrap pair and the loader's empty call share:
/// names plus one-line descriptions — enough to pick, never enough to
/// call.
fn catalog_text(tools: &[MountedTool]) -> String {
    let mut text = format!(
        "Mounted MCP tools (schemas are deferred — call {LOADER_TOOL_NAME} with the names you need before calling one):"
    );
    for tool in tools {
        text.push_str("\n- ");
        text.push_str(&tool.name);
        let line = one_line(&tool.description);
        if !line.is_empty() {
            text.push_str(": ");
            text.push_str(&line);
        }
    }
    text
}

/// The loader tool over one Turn's snapshot. `names` loads full
/// definitions and marks them through `added_tool_names`; an empty or
/// absent list returns the catalog. Unknown names report against the
/// mounted set — the config file may have changed since the snapshot.
pub(crate) fn loader_tool(tools: &[AgentTool]) -> AgentTool {
    let snapshot = mounted(tools);
    AgentTool {
        name: LOADER_TOOL_NAME.to_string(),
        label: "MCP tools".to_string(),
        description: format!(
            "List the mounted MCP tools, or load the full definitions (description and \
             parameter schema) of the named mcp__* tools so they can be called. MCP tool \
             schemas are deferred: load a tool before its first call, at most \
             {MAX_LOAD_PER_CALL} per call."
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Mounted MCP tool names to load (e.g. [\"mcp__github__create_issue\"]). Omit or pass an empty array to return the catalog."
                }
            }
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  _signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let snapshot = snapshot.clone();
                let params = params.clone();
                Box::pin(async move {
                    let mut requested: Vec<String> = Vec::new();
                    if let Some(array) = params.get("names").and_then(|value| value.as_array()) {
                        for name in array.iter().filter_map(|value| value.as_str()) {
                            let name = name.trim();
                            if !name.is_empty() && !requested.iter().any(|seen| seen == name) {
                                requested.push(name.to_string());
                            }
                        }
                    }
                    if requested.is_empty() {
                        return Ok(AgentToolResult {
                            content: vec![BlockContent::Text(TextContent {
                                text: catalog_text(&snapshot),
                                ..Default::default()
                            })],
                            ..Default::default()
                        });
                    }
                    let skipped = requested.len().saturating_sub(MAX_LOAD_PER_CALL);
                    let mut loaded: Vec<&MountedTool> = Vec::new();
                    let mut unknown: Vec<&str> = Vec::new();
                    for name in requested.iter().take(MAX_LOAD_PER_CALL) {
                        match snapshot.iter().find(|tool| &tool.name == name) {
                            Some(tool) => loaded.push(tool),
                            None => unknown.push(name),
                        }
                    }
                    if loaded.is_empty() {
                        return Err(format!(
                            "none of the requested tools are mounted in this Turn; call \
                             {LOADER_TOOL_NAME} with no names for the catalog"
                        ));
                    }
                    let mut text = String::new();
                    for tool in &loaded {
                        text.push_str(&format!(
                            "## {}\n\n{}\n\nParameters:\n```json\n{}\n```\n\n",
                            tool.name,
                            tool.description,
                            serde_json::to_string_pretty(&tool.parameters)
                                .unwrap_or_else(|_| tool.parameters.to_string())
                        ));
                    }
                    if !unknown.is_empty() {
                        text.push_str(&format!(
                            "Not mounted in this Turn: {}.\n",
                            unknown.join(", ")
                        ));
                    }
                    if skipped > 0 {
                        text.push_str(&format!(
                            "…[{skipped} more names skipped; load at most {MAX_LOAD_PER_CALL} per call]\n"
                        ));
                    }
                    Ok(AgentToolResult {
                        content: vec![BlockContent::Text(TextContent {
                            text: text.trim_end().to_string(),
                            ..Default::default()
                        })],
                        details: serde_json::json!({
                            "loaded": loaded.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
                        }),
                        added_tool_names: Some(
                            loaded.iter().map(|tool| tool.name.clone()).collect(),
                        ),
                        ..Default::default()
                    })
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// The synthetic pair every request of the Turn appends to the LLM view
/// (ADR-0036): a loader call plus its catalog result, whose
/// `added_tool_names` marks every mounted MCP tool deferred from the very
/// first request. LLM-view only — never persisted to the transcript, so a
/// Turn restart simply re-appends it.
pub(crate) fn bootstrap_messages(tools: &[AgentTool], model: &Model) -> Vec<Message> {
    let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let call_id = "holt-mcp-bootstrap";
    vec![
        Message::Assistant(Box::new(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: call_id.to_string(),
                name: LOADER_TOOL_NAME.to_string(),
                ..Default::default()
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            timestamp: Utc::now().timestamp_millis(),
            ..Default::default()
        })),
        Message::ToolResult(Box::new(ToolResultMessage {
            tool_call_id: call_id.to_string(),
            tool_name: LOADER_TOOL_NAME.to_string(),
            content: vec![BlockContent::Text(TextContent {
                text: catalog_text(&mounted(tools)),
                ..Default::default()
            })],
            added_tool_names: Some(names),
            is_error: false,
            timestamp: Utc::now().timestamp_millis(),
            ..Default::default()
        })),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_truncate_at_two_kilobytes_with_a_marker() {
        let short = truncate_description("short description");
        assert_eq!(short, "short description");
        let verbose = "v".repeat(3 * 1024);
        let truncated = truncate_description(&verbose);
        assert!(
            truncated.len() < verbose.len(),
            "the 3 KB description must shrink"
        );
        assert!(
            truncated.contains("description truncated at 2048 bytes"),
            "marker missing: {}…",
            truncated.chars().rev().take(80).collect::<String>()
        );
        assert!(truncated.starts_with(&verbose[..1024]));
        // Exactly-at-the-cap text passes untouched.
        let exact = "d".repeat(MAX_DESCRIPTION_BYTES);
        assert_eq!(truncate_description(&exact), exact);
        // Multibyte characters cut on a char boundary, never mid-codepoint.
        let multibyte = "é".repeat(MAX_DESCRIPTION_BYTES);
        let truncated = truncate_description(&multibyte);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn results_cap_at_one_hundred_thousand_characters_with_a_marker() {
        let fine = truncate_result("small");
        assert_eq!(fine, "small");
        let huge: String = "x".repeat(MAX_RESULT_CHARS + 5_000);
        let capped = truncate_result(&huge);
        assert!(
            capped.contains("result truncated at 100000 characters"),
            "marker missing"
        );
        let without_marker_len = capped.find("\n…[").unwrap();
        assert_eq!(without_marker_len, MAX_RESULT_CHARS);
        let exact: String = "y".repeat(MAX_RESULT_CHARS);
        assert_eq!(truncate_result(&exact).chars().count(), MAX_RESULT_CHARS);
    }

    #[test]
    fn tool_names_validate_the_two_level_carrier() {
        assert!(tool_name_is_legal("echo"));
        assert!(tool_name_is_legal("a-b_C9"));
        assert!(tool_name_is_legal(&"n".repeat(64)));
        // Illegal: empty, overlong, or outside [A-Za-z0-9_-].
        assert!(!tool_name_is_legal(""));
        assert!(!tool_name_is_legal(&"n".repeat(65)));
        assert!(!tool_name_is_legal("bad name"));
        assert!(!tool_name_is_legal("bad/name"));
    }

    fn fake_tool(name: &str, description: &str) -> AgentTool {
        AgentTool {
            name: name.to_string(),
            label: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } }
            }),
            constrained_sampling: None,
            prepare_arguments: None,
            execution_mode: None,
            execute: Arc::new(|_, _, _, _| {
                Box::pin(async {
                    Ok(AgentToolResult {
                        content: vec![],
                        details: serde_json::Value::Object(serde_json::Map::new()),
                        usage: None,
                        added_tool_names: None,
                        terminate: None,
                    })
                }) as BoxFuture<'static, Result<AgentToolResult, String>>
            }),
        }
    }

    fn result_text(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .map(|block| match block {
                BlockContent::Text(text) => text.text.clone(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn catalog_lists_names_with_one_line_descriptions() {
        let tools = vec![
            fake_tool(
                "mcp__github__create_issue",
                "Create an issue.\nSecond line is dropped.",
            ),
            fake_tool("mcp__github__search", "  spaced   out  "),
            fake_tool("mcp__bare__noop", ""),
        ];
        let catalog = catalog_text(&mounted(&tools));
        assert!(catalog.contains("- mcp__github__create_issue: Create an issue."));
        assert!(!catalog.contains("Second line"));
        assert!(catalog.contains("- mcp__github__search: spaced out"));
        assert!(catalog.trim_end().ends_with("- mcp__bare__noop"));
        // Long first lines truncate at the cap with a marker.
        let long = fake_tool("mcp__s__long", &"d".repeat(200));
        let catalog = catalog_text(&mounted(&[long]));
        assert!(catalog.contains(&format!("{}…", "d".repeat(CATALOG_DESCRIPTION_CHARS))));
    }

    #[tokio::test]
    async fn loader_without_names_returns_the_catalog() {
        let loader = loader_tool(&[fake_tool("mcp__a__x", "Does x.")]);
        let result = (loader.execute)("call-1", &serde_json::json!({}), None, None)
            .await
            .unwrap();
        assert!(result.added_tool_names.is_none());
        assert!(result_text(&result).contains("- mcp__a__x: Does x."));
    }

    #[tokio::test]
    async fn loader_returns_definitions_and_marks_loaded() {
        let tools = vec![fake_tool("mcp__github__create_issue", "Create an issue.")];
        let loader = loader_tool(&tools);
        let result = (loader.execute)(
            "call-1",
            &serde_json::json!({"names": ["mcp__github__create_issue"]}),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            result.added_tool_names,
            Some(vec!["mcp__github__create_issue".to_string()])
        );
        let text = result_text(&result);
        assert!(text.contains("## mcp__github__create_issue"));
        assert!(text.contains("Create an issue."));
        assert!(text.contains("\"q\""), "schema missing: {text}");
    }

    #[tokio::test]
    async fn loader_reports_unknown_names_against_the_mounted_set() {
        let loader = loader_tool(&[fake_tool("mcp__a__x", "x")]);
        let error = (loader.execute)(
            "call-1",
            &serde_json::json!({"names": ["mcp__gone__y"]}),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("catalog"), "unexpected error: {error}");
        // A mixed call still loads the known half and reports the rest.
        let result = (loader.execute)(
            "call-2",
            &serde_json::json!({"names": ["mcp__a__x", "mcp__gone__y"]}),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.added_tool_names, Some(vec!["mcp__a__x".to_string()]));
        assert!(result_text(&result).contains("Not mounted in this Turn: mcp__gone__y"));
    }

    #[tokio::test]
    async fn loader_caps_each_call_and_reports_the_skip() {
        let tools: Vec<AgentTool> = (0..10)
            .map(|index| fake_tool(&format!("mcp__s__t{index}"), "d"))
            .collect();
        let loader = loader_tool(&tools);
        let names: Vec<String> = (0..10).map(|index| format!("mcp__s__t{index}")).collect();
        let result = (loader.execute)("call-1", &serde_json::json!({"names": names}), None, None)
            .await
            .unwrap();
        assert_eq!(
            result.added_tool_names.as_ref().unwrap().len(),
            MAX_LOAD_PER_CALL
        );
        assert!(result_text(&result).contains("2 more names skipped"));
    }

    #[test]
    fn bootstrap_pair_marks_every_mounted_tool() {
        let tools = vec![fake_tool("mcp__a__x", "x"), fake_tool("mcp__b__y", "y")];
        let pair = bootstrap_messages(&tools, &Model::default());
        assert_eq!(pair.len(), 2);
        let Message::Assistant(call) = &pair[0] else {
            panic!("first message must be the assistant call")
        };
        let Message::ToolResult(result) = &pair[1] else {
            panic!("second message must be the tool result")
        };
        let Some(AssistantContent::ToolCall(tool_call)) = call.content.first() else {
            panic!("the assistant message must carry the loader call")
        };
        assert_eq!(tool_call.name, LOADER_TOOL_NAME);
        assert_eq!(result.tool_call_id, tool_call.id);
        assert_eq!(result.tool_name, LOADER_TOOL_NAME);
        assert!(!result.is_error);
        assert_eq!(
            result.added_tool_names,
            Some(vec!["mcp__a__x".to_string(), "mcp__b__y".to_string()])
        );
    }

    #[test]
    fn the_bootstrap_pair_defers_mounted_tools_until_called() {
        use pi_core::ai::{
            types::{Context, Tool},
            utils::deferred_tools::split_deferred_tools,
        };

        let mounted_tools = vec![fake_tool("mcp__a__x", "x")];
        let toolset = [
            fake_tool("read", "read"),
            loader_tool(&mounted_tools),
            fake_tool("mcp__a__x", "x"),
        ];
        let provider_tools: Vec<Tool> = toolset.iter().map(|tool| tool.to_tool()).collect();
        let pair = bootstrap_messages(&mounted_tools, &Model::default());
        let context = Context {
            messages: pair.clone(),
            tools: Some(provider_tools.clone()),
            ..Default::default()
        };
        let split = split_deferred_tools(&context, true, |name| name.to_string());
        assert!(split.deferred.contains_key("mcp__a__x"));
        assert_eq!(split.deferred.len(), 1);
        let immediate: Vec<&str> = split
            .immediate
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(immediate, vec!["read", LOADER_TOOL_NAME]);

        // Once the assistant calls the tool it turns immediate: the pair
        // always appends after the conversation, so the call precedes the
        // marker and the schema joins the static declarations.
        let mut messages = vec![Message::Assistant(Box::new(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call-2".to_string(),
                name: "mcp__a__x".to_string(),
                ..Default::default()
            })],
            ..Default::default()
        }))];
        messages.extend(pair);
        let context = Context {
            messages,
            tools: Some(provider_tools),
            ..Default::default()
        };
        let split = split_deferred_tools(&context, true, |name| name.to_string());
        assert!(split.deferred.is_empty());
    }
}

/// What a probe learned about one server.
#[derive(Debug)]
pub(crate) enum ProbeReport {
    Ok {
        tool_count: usize,
        tool_names: Vec<String>,
    },
    Failed {
        reason: String,
    },
}
