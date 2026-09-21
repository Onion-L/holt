//! The MCP stdio test fixture (ADR-0034): a minimal in-repo MCP server
//! speaking newline-delimited JSON-RPC over stdin/stdout — initialize,
//! `tools/list`, and `tools/call` — exactly as production config would
//! spawn one, exercising the real SDK transport end to end. Behavior knobs
//! arrive through environment variables so the same binary serves every
//! engine test:
//!
//! - `FIXTURE_MARKER`: a path touched once the process starts — the lazy-
//!   start and never-started assertions watch it.
//! - `FIXTURE_LIST_CHANGED_AFTER`: emit `notifications/tools/list_changed`
//!   after this many `tools/list` calls (ticket 03).
//! - `FIXTURE_EXIT_ON_CALL`: exit the process when the Nth `tools/call`
//!   arrives (ticket 03's mid-Turn death).
//! - `FIXTURE_STALL_INITIALIZE`: never answer `initialize` — the startup
//!   timeout's hang stand-in (ticket 03).
//! - `FIXTURE_SLOW_CALL_MS`: sleep this many ms before answering each
//!   `tools/call` (ticket 03's per-call timeout).
//! - `FIXTURE_ONE_TOOL_FIRST_LIST`: the first `tools/list` answers with
//!   only `echo`; later lists answer with the full set (ticket 03's
//!   listChanged-lands-next-Turn).
//! - `FIXTURE_VERBOSE_DESCRIPTION`: `echo`'s description grows to 3 KB
//!   (ticket 05's description ceiling).
//! - `FIXTURE_BIG_RESULT`: add a `big` tool answering 120k characters plus
//!   a tail text block and an image block (ticket 05's result ceiling).
//! - `FIXTURE_PAGINATE`: serve `tools/list` one tool per page with
//!   `nextCursor` (ticket 05's pagination).
//! - `FIXTURE_BAD_TOOL_NAME`: list a tool with an illegal name
//!   (ticket 05's connect-time refusal).
//!
//! stdout carries protocol messages only; diagnostics go to stderr.

use std::{
    io::{BufRead, Write},
    process::exit,
};

fn main() {
    if let Ok(marker) = std::env::var("FIXTURE_MARKER") {
        let _ = std::fs::write(&marker, std::process::id().to_string());
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => exit(0),
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: serde_json::Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("fixture: unparsable line: {error}");
                continue;
            }
        };
        if let Err(error) = serve(&message) {
            eprintln!("fixture: {error}");
        }
        if message["method"].as_str() == Some("tools/call")
            && let Ok(exit_after) = std::env::var("FIXTURE_EXIT_ON_CALL")
        {
            let calls = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if calls.to_string() == exit_after {
                eprintln!("fixture: exiting on call {calls}");
                exit(9);
            }
        }
    }
}

static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LIST_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn send(value: serde_json::Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", value);
    let _ = stdout.flush();
}

fn reply(id: &serde_json::Value, result: serde_json::Value) {
    send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }));
}

fn error_reply(id: &serde_json::Value, code: i64, message: &str) {
    send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }));
}

fn serve(message: &serde_json::Value) -> Result<(), String> {
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let id = message.get("id").cloned();
    match (method.as_str(), id) {
        ("initialize", Some(id)) => {
            if std::env::var("FIXTURE_STALL_INITIALIZE").is_ok() {
                // Hang the handshake: the client's startup timeout is the
                // behavior under test.
                return Ok(());
            }
            // Echo the requested protocol version: the fixture speaks
            // whatever the client asked for.
            let version = message["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2025-06-18")
                .to_string();
            reply(
                &id,
                serde_json::json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "holt-fixture", "version": "0.0.0" },
                }),
            );
        }
        ("notifications/initialized", None) => {}
        ("ping", Some(id)) => reply(&id, serde_json::json!({})),
        ("tools/list", Some(id)) => {
            let count = LIST_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let all = tools();
            let one_tool = std::env::var("FIXTURE_ONE_TOOL_FIRST_LIST").is_ok() && count == 0;
            let paginate = std::env::var("FIXTURE_PAGINATE").is_ok();
            let offset: usize = message["params"]["cursor"]
                .as_str()
                .and_then(|cursor| cursor.parse().ok())
                .unwrap_or(0);
            let listed: Vec<serde_json::Value> = if one_tool {
                all[0..1].to_vec()
            } else if paginate {
                // One tool per page: proves the engine follows `nextCursor`.
                all.get(offset..offset + 1)
                    .map(|page| page.to_vec())
                    .unwrap_or_default()
            } else {
                all.clone()
            };
            let mut result = serde_json::json!({ "tools": listed });
            if paginate && offset + 1 < all.len() {
                result["nextCursor"] = serde_json::json!((offset + 1).to_string());
            }
            reply(&id, result);
            if let Ok(after) = std::env::var("FIXTURE_LIST_CHANGED_AFTER")
                && count >= after.parse::<usize>().unwrap_or(0)
            {
                send(serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                }));
            }
        }
        ("tools/call", Some(id)) => {
            if let Ok(slow) = std::env::var("FIXTURE_SLOW_CALL_MS") {
                let ms: u64 = slow.parse().unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            let name = message["params"]["name"].as_str().unwrap_or_default();
            let arguments = message["params"]["arguments"].clone();
            match name {
                "echo" => {
                    let text = arguments["message"].as_str().unwrap_or_default();
                    reply(
                        &id,
                        serde_json::json!({
                            "content": [ { "type": "text", "text": format!("echo: {text}") } ],
                        }),
                    );
                }
                "fail" => {
                    let text = arguments["message"].as_str().unwrap_or("fixture failure");
                    reply(
                        &id,
                        serde_json::json!({
                            "isError": true,
                            "content": [ { "type": "text", "text": format!("fixture error: {text}") } ],
                        }),
                    );
                }
                "big" => {
                    let mut text = String::with_capacity(120_000);
                    while text.len() < 120_000 {
                        text.push_str("0123456789abcdef");
                    }
                    // A second text block plus an image block: joining and
                    // the non-text notice ride the same result.
                    reply(
                        &id,
                        serde_json::json!({
                            "content": [
                                { "type": "text", "text": text },
                                { "type": "text", "text": "tail block" },
                                { "type": "image", "data": "aGk=", "mimeType": "image/png" },
                            ],
                        }),
                    );
                }
                other => error_reply(&id, -32602, &format!("unknown tool {other:?}")),
            }
        }
        (other, Some(id)) => error_reply(&id, -32601, &format!("unknown method {other:?}")),
        _ => {}
    }
    Ok(())
}

fn tools() -> Vec<serde_json::Value> {
    let verbose = std::env::var("FIXTURE_VERBOSE_DESCRIPTION").is_ok();
    let description = if verbose {
        // 3 KB of description — the engine's 2 KB ceiling is under test.
        "v".repeat(3 * 1024)
    } else {
        "Return the message prefixed with `echo: `.".to_string()
    };
    let mut tools = vec![
        serde_json::json!({
            "name": "echo",
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "The text to return." },
                },
                "required": ["message"],
                "additionalProperties": false,
            },
        }),
        serde_json::json!({
            "name": "fail",
            "description": "Settle as an `isError` result carrying the message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                },
                "required": ["message"],
                "additionalProperties": false,
            },
        }),
    ];
    if std::env::var("FIXTURE_BIG_RESULT").is_ok() {
        tools.push(serde_json::json!({
            "name": "big",
            "description": "Return a result past the engine's 100k-character ceiling.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            },
        }));
    }
    if std::env::var("FIXTURE_BAD_TOOL_NAME").is_ok() {
        tools.push(serde_json::json!({
            "name": "bad name!",
            "description": "An illegal name the engine must refuse verbatim.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            },
        }));
    }
    tools
}
