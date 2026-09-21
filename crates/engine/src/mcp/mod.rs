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

pub(crate) mod config;

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult, Tool},
    service::{RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use config::{McpServer, McpStore, ServerTransport};

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
    /// admission by main-chat runs; the definition set is read fresh, so
    /// a hand-edited config lands from the next Turn.
    pub(crate) async fn agent_tools(&self) -> Vec<AgentTool> {
        let mut tools = Vec::new();
        let mut connections = self.connections.lock().await;
        for (name, server) in self.servers.get() {
            if !server.enabled {
                continue;
            }
            // A cached connection whose list has gone stale (a child that
            // died since the last Turn) is dropped and reconnected in the
            // same pass — a server restarted between Turns works again on
            // the very next one, no app restart (ADR-0034).
            let mut fresh: Option<(Arc<LiveServer>, ListToolsResult)> = None;
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
                list.tools
                    .into_iter()
                    .map(|tool| wrap_server_tool(&name, tool, &connection)),
            );
        }
        tools
    }
}

/// Connect (or reconnect) one server and list its tools, caching the
/// live connection on success. `None` — with a log line — skips the
/// server for this Turn.
async fn connect_fresh(
    connections: &mut HashMap<String, Arc<LiveServer>>,
    name: &str,
    server: &McpServer,
) -> Option<(Arc<LiveServer>, ListToolsResult)> {
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
    /// `tools/list`, one page (pagination following is ticket 05).
    async fn list_tools(&self) -> Result<ListToolsResult, String> {
        self.service
            .list_tools(None)
            .await
            .map_err(|error| service_error(&error))
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
}

fn service_error(error: &ServiceError) -> String {
    format!("mcp server error: {error}")
}

/// Connect one server per its definition. The startup timeout bounds the
/// initialize handshake; expansion and environment sanitization ride the
/// config layer (ticket 06).
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
            let mut process = tokio::process::Command::new(command);
            process.args(args).envs(env);
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
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| format!("header name {name:?} is invalid: {error}"))?;
                let value = reqwest::header::HeaderValue::from_str(value)
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

/// Wrap one listed server tool as a standard agent tool (ADR-0034): the
/// two-level name, the description and input schema as received, execute
/// forwarding to `tools/call` under the server's call timeout. Text
/// content blocks join with newlines; `isError` results settle as error
/// results the model reads.
fn wrap_server_tool(server: &str, tool: Tool, connection: &Arc<LiveServer>) -> AgentTool {
    let tool_name = tool.name.to_string();
    let label = tool
        .title
        .clone()
        .unwrap_or_else(|| format!("MCP {server} · {tool_name}"));
    let description = tool.description.as_deref().unwrap_or_default().to_string();
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
                    let text = join_text_content(&result);
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

/// Text content blocks joined with newlines; non-text blocks (images,
/// audio, embedded resources) drop with an in-band notice, and
/// `structuredContent` is ignored (ADR-0034).
fn join_text_content(result: &CallToolResult) -> String {
    let mut parts = Vec::new();
    let mut dropped_non_text = false;
    for block in &result.content {
        match block {
            rmcp::model::ContentBlock::Text(text) => parts.push(text.text.clone()),
            _ => dropped_non_text = true,
        }
    }
    let mut text = parts.join("\n");
    if dropped_non_text {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[non-text content blocks were dropped]");
    }
    text
}
