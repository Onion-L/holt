//! Streamable HTTP MCP transport at the RPC seam (ADR-0034, ticket 04):
//! an `http` config entry (URL + static headers, optionally a bearer token
//! referenced by environment variable name) connects over the real
//! Streamable HTTP transport, mounts tools like a stdio server, and keeps
//! the ticket-03 failure semantics. The loopback listener below speaks
//! just enough MCP-over-HTTP: JSON POSTs answered with JSON, the
//! `initialized` notification with 202, and an open-but-silent SSE stream
//! for the client's GET.

mod common;

use common::{ScriptedProvider, ScriptedReply};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn write_mcp_config(data_dir: &Path, server: serde_json::Value) {
    std::fs::write(
        data_dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": { "remote": server } }))
            .unwrap(),
    )
    .unwrap();
}

/// One MCP-over-HTTP reply: a JSON body with 200, or an empty 202.
enum HttpReply {
    Json(serde_json::Value),
    Accepted,
    /// The client's GET stream: headers only, then silence.
    EventStream,
}

/// The loopback MCP server. Captures every request head it saw — the seam
/// that proves what actually left the process (the bearer token).
#[derive(Clone)]
struct LoopbackMcp {
    base: String,
    heads: Arc<Mutex<Vec<String>>>,
}

async fn serve_loopback_mcp() -> LoopbackMcp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/mcp", listener.local_addr().unwrap());
    let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = heads.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(socket, sink).await {
                    eprintln!("loopback mcp: {error}");
                }
            });
        }
    });
    LoopbackMcp { base, heads }
}

async fn read_head(
    socket: &mut tokio::net::TcpStream,
) -> Result<(String, std::collections::HashMap<String, String>, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = socket
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before the head".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_head_end(&buffer) {
            break position;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = socket
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok((request_line, headers, body))
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_reply(socket: &mut tokio::net::TcpStream, reply: HttpReply) {
    let (head, body) = match reply {
        HttpReply::Json(value) => {
            let body = value.to_string();
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                body,
            )
        }
        HttpReply::Accepted => (
            "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            String::new(),
        ),
        HttpReply::EventStream => (
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\nCache-Control: no-cache\r\n\r\n"
                .to_string(),
            String::new(),
        ),
    };
    let _ = socket.write_all(head.as_bytes()).await;
    if !body.is_empty() {
        let _ = socket.write_all(body.as_bytes()).await;
    }
    let _ = socket.flush().await;
}

async fn serve_connection(
    mut socket: tokio::net::TcpStream,
    heads: Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    loop {
        let (request_line, headers, body) = read_head(&mut socket).await?;
        {
            let mut sink = heads.lock().unwrap();
            sink.push(format!(
                "{request_line} | authorization: {} | x-static: {}",
                headers
                    .get("authorization")
                    .map(String::as_str)
                    .unwrap_or("-"),
                headers.get("x-static").map(String::as_str).unwrap_or("-"),
            ));
        }
        if !request_line.starts_with("POST") {
            // The client's standing GET stream: headers, then silence.
            write_reply(&mut socket, HttpReply::EventStream).await;
            // Hold the stream open until the client goes away.
            let mut sink = [0u8; 64];
            let _ = socket.read(&mut sink).await;
            return Ok(());
        }
        let message: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let id = message.get("id").cloned();
        match (method.as_str(), id) {
            ("initialize", Some(id)) => {
                let version = message["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or("2025-06-18")
                    .to_string();
                write_reply(
                    &mut socket,
                    HttpReply::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": version,
                            "capabilities": { "tools": { "listChanged": false } },
                            "serverInfo": { "name": "loopback", "version": "0.0.0" },
                        }
                    })),
                )
                .await;
            }
            ("notifications/initialized", None) => {
                write_reply(&mut socket, HttpReply::Accepted).await;
            }
            ("tools/list", Some(id)) => {
                write_reply(
                    &mut socket,
                    HttpReply::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "tools": [ {
                            "name": "http_echo",
                            "description": "Return the message over http.",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "message": { "type": "string" } },
                                "required": ["message"],
                                "additionalProperties": false,
                            }
                        } ] }
                    })),
                )
                .await;
            }
            ("tools/call", Some(id)) => {
                let text = message["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or_default();
                write_reply(
                    &mut socket,
                    HttpReply::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [ { "type": "text", "text": format!("http echo: {text}") } ],
                        }
                    })),
                )
                .await;
            }
            (other, Some(id)) => {
                write_reply(
                    &mut socket,
                    HttpReply::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown method {other}") },
                    })),
                )
                .await;
            }
            _ => {
                write_reply(&mut socket, HttpReply::Accepted).await;
            }
        }
    }
}

#[tokio::test]
async fn an_http_server_mounts_and_runs_over_streamable_http() {
    let fixture = common::Fixture::new();
    let server = serve_loopback_mcp().await;
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "url": server.base,
            "headers": { "X-Static": "one" },
        }),
    );
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "mcp__remote__http_echo",
            serde_json::json!({ "message": "over http" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "use the http tool").await;

    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let requests = provider.requests();
    assert!(
        requests[0]
            .tool_names
            .contains(&"mcp__remote__http_echo".to_string()),
        "toolset: {:?}",
        requests[0].tool_names
    );
    let last = common::summarize(&requests.last().unwrap().messages);
    assert!(
        last.iter().any(
            |row| row.starts_with("toolresult:call-1:") && row.contains("http echo: over http")
        ),
        "the http call's result never reached the model: {last:?}"
    );
    // The static header actually left the process.
    assert!(
        server
            .heads
            .lock()
            .unwrap()
            .iter()
            .any(|head| head.contains("x-static: one")),
        "heads: {:?}",
        server.heads.lock().unwrap()
    );
}

#[tokio::test]
async fn a_bearer_token_env_var_authenticates_without_landing_anywhere() {
    let fixture = common::Fixture::new();
    let server = serve_loopback_mcp().await;
    unsafe { std::env::set_var("HOLT_MCP_TEST_TOKEN", "token-value-xyz") };
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "url": server.base,
            "bearerTokenEnvVar": "HOLT_MCP_TEST_TOKEN",
        }),
    );
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "call-1",
            "mcp__remote__http_echo",
            serde_json::json!({ "message": "authed" }),
        ),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "use the authed tool").await;

    common::wait_for_transcript_text(&mut transcript, "done").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    // The named variable's value rode the Authorization header.
    let heads = server.heads.lock().unwrap().clone();
    assert!(
        heads
            .iter()
            .any(|head| head.contains("authorization: Bearer token-value-xyz")),
        "heads: {heads:?}"
    );
    // …and never the config file.
    let config = std::fs::read_to_string(fixture.data_dir.path().join("mcp.json")).unwrap();
    assert!(!config.contains("token-value-xyz"), "config: {config}");
    // …and never the transcript.
    let snapshot = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        !snapshot.to_string().contains("token-value-xyz"),
        "token leaked into the transcript"
    );
}

#[tokio::test]
async fn an_unreachable_http_server_is_skipped_and_the_turn_proceeds() {
    let fixture = common::Fixture::new();
    // Bind then drop a listener to claim a free — and closed — port.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!(
        "http://127.0.0.1:{}/mcp",
        probe.local_addr().unwrap().port()
    );
    drop(probe);
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({ "url": dead_url }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "the unreachable server's tools must be absent: {:?}",
        requests[0].tool_names
    );
}

#[tokio::test]
async fn a_missing_bearer_env_var_skips_the_server() {
    let fixture = common::Fixture::new();
    let server = serve_loopback_mcp().await;
    unsafe { std::env::remove_var("HOLT_MCP_ABSENT_TOKEN") };
    write_mcp_config(
        fixture.data_dir.path(),
        serde_json::json!({
            "url": server.base,
            "bearerTokenEnvVar": "HOLT_MCP_ABSENT_TOKEN",
        }),
    );
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("kept going")]);
    let engine = fixture.engine(&provider);
    common::setup_ungated_chat(&engine, "chat-1").await;
    let (mut transcript, mut sessions) = common::subscribe(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "hello").await;

    common::wait_for_transcript_text(&mut transcript, "kept going").await;
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;
    let requests = provider.requests();
    assert!(
        !requests[0]
            .tool_names
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "toolset: {:?}",
        requests[0].tool_names
    );
}
