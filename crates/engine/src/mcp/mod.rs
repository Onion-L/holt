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
    transport::TokioChildProcess,
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
            let connection = match connections.get(&name) {
                Some(connection) => Arc::clone(connection),
                None => match connect(&server).await {
                    Ok(connection) => {
                        let connection = Arc::new(connection);
                        connections.insert(name.clone(), Arc::clone(&connection));
                        connection
                    }
                    // A server that cannot start is skipped for this Turn
                    // (log line, tools absent); the next Turn retries.
                    Err(error) => {
                        tracing::warn!(
                            target: "holt::mcp",
                            server = %name,
                            %error,
                            "mcp server failed to connect; skipping it for this turn"
                        );
                        continue;
                    }
                },
            };
            match connection.list_tools().await {
                Ok(list) => tools.extend(
                    list.tools
                        .into_iter()
                        .map(|tool| wrap_server_tool(&name, tool, &connection)),
                ),
                Err(error) => {
                    tracing::warn!(
                        target: "holt::mcp",
                        server = %name,
                        %error,
                        "mcp server failed to list tools; skipping it for this turn"
                    );
                    // Drop the dead entry so the next Turn reconnects.
                    connections.remove(&name);
                }
            }
        }
        tools
    }
}

/// One live server connection: the running client service over its
/// transport. Holding it keeps the child (or HTTP session) alive; dropping
/// the last reference shuts it down.
struct LiveServer {
    service: RunningService<RoleClient, ()>,
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
        let response = self
            .service
            .call_tool_once(params)
            .await
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
            Ok(LiveServer { service })
        }
        ServerTransport::Http { .. } => {
            Err("http MCP servers are not supported yet (ticket 04)".into())
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
