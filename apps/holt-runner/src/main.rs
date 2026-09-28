//! holt-runner — one headless Turn against the local engine, for eval harnesses and scripts.
//!
//! Assembles `LocalEngine` in a fresh data dir (seeded with the provider
//! catalog and credentials from the user's data dir), creates one
//! full-access chat, queues the prompt, waits for the Turn's terminal event,
//! and prints a JSON summary on stdout. No UI; the engine is driven only
//! through the RPC contract, exactly as the shell drives it. The transcript
//! stays under `<data-dir>/transcripts/` for the caller to read.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use futures::StreamExt as _;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService as _, methods};
use serde_json::{Value, json};

const CHAT_ID: &str = "run";
const MESSAGE_ID: &str = "run-prompt";
const COMMIT: &str = env!("HOLT_RUNNER_COMMIT");

/// Files copied from the source data dir: the provider catalog and keys.
/// Web search, MCP, and title settings stay out so a run mounts no network
/// tools and spends tokens only on the Turn itself.
const SEEDED: &[&str] = &[
    "provider-store.json",
    "provider-settings.json",
    "provider-credentials.json",
];

const USAGE: &str = "usage: holt-runner --cwd <dir> --prompt-file <file> --model <provider/model> \
--data-dir <fresh dir> [--source-data-dir <dir>] [--reasoning <level>] [--timeout-sec <n>]";

struct Args {
    cwd: PathBuf,
    prompt: String,
    model: String,
    data_dir: PathBuf,
    source_data_dir: PathBuf,
    reasoning: Option<String>,
    timeout: Duration,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut cwd = None;
    let mut prompt_file = None;
    let mut model = None;
    let mut data_dir = None;
    let mut source_data_dir = None;
    let mut reasoning = None;
    let mut timeout_sec = 1800u64;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--cwd" => cwd = Some(PathBuf::from(value()?)),
            "--prompt-file" => prompt_file = Some(PathBuf::from(value()?)),
            "--model" => model = Some(value()?),
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--source-data-dir" => source_data_dir = Some(PathBuf::from(value()?)),
            "--reasoning" => reasoning = Some(value()?),
            "--timeout-sec" => {
                timeout_sec = value()?
                    .parse()
                    .map_err(|_| anyhow!("--timeout-sec needs a whole number\n{USAGE}"))?;
            }
            "-V" | "--version" => {
                println!("holt-runner {} ({COMMIT})", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    let prompt_file = prompt_file.ok_or_else(|| anyhow!("--prompt-file is required\n{USAGE}"))?;
    let source_data_dir = match source_data_dir {
        Some(dir) => dir,
        None => match std::env::var_os("HOLT_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?).join(".holt"),
        },
    };
    Ok(Args {
        cwd: cwd
            .ok_or_else(|| anyhow!("--cwd is required\n{USAGE}"))?
            .canonicalize()?,
        prompt: std::fs::read_to_string(&prompt_file)
            .with_context(|| format!("reading {}", prompt_file.display()))?,
        model: model.ok_or_else(|| anyhow!("--model is required\n{USAGE}"))?,
        data_dir: data_dir.ok_or_else(|| anyhow!("--data-dir is required\n{USAGE}"))?,
        source_data_dir,
        reasoning,
        timeout: Duration::from_secs(timeout_sec),
    })
}

fn seed_data_dir(source: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    if std::fs::read_dir(dest)?.next().is_some() {
        bail!("--data-dir {} must be empty", dest.display());
    }
    for name in SEEDED {
        let from = source.join(name);
        if from.exists() {
            std::fs::copy(&from, dest.join(name))
                .with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

async fn call(engine: &LocalEngine, method: &str, params: Value) -> anyhow::Result<Value> {
    match engine
        .handle(method, params)
        .await
        .map_err(|error| anyhow!("{method}: {error:?}"))?
    {
        RpcReply::Value(value) => Ok(value),
        RpcReply::Stream(_) => bail!("{method} replied with a stream"),
    }
}

async fn watch(
    engine: &LocalEngine,
    method: &str,
    params: Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Value>> {
    match engine
        .handle(method, params)
        .await
        .map_err(|error| anyhow!("{method}: {error:?}"))?
    {
        RpcReply::Stream(stream) => Ok(stream),
        RpcReply::Value(_) => bail!("{method} replied with a value"),
    }
}

/// The prompt's queue-level rejection: its own pending error, or the queue
/// pausing on an error while the prompt was never started.
fn queue_error(frame: &Value) -> Option<String> {
    let pending = frame["pending"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["messageId"] == MESSAGE_ID));
    if let Some(error) = pending.and_then(|item| item["error"].as_str()) {
        return Some(error.to_owned());
    }
    let started = frame["activeMessageId"] == MESSAGE_ID;
    if frame["paused"] == true && !started {
        return frame["error"].as_str().map(str::to_owned);
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args()?;
    let (provider, _) = args
        .model
        .split_once('/')
        .ok_or_else(|| anyhow!("--model must be provider-qualified (provider/model)"))?;
    seed_data_dir(&args.source_data_dir, &args.data_dir)?;
    let engine = LocalEngine::assemble(&EngineConfig {
        data_dir: args.data_dir.clone(),
        personal_skills_dir: Some(args.data_dir.join("personal-skills")),
        stream_fn: None,
        search_backend_resolver: None,
    })
    .map_err(|error| anyhow!("assembling the engine: {error}"))?;

    call(
        &engine,
        methods::MUTATE,
        json!({"op": "createChat", "chatId": CHAT_ID}),
    )
    .await?;
    call(
        &engine,
        methods::MUTATE,
        json!({"op": "setChatPermissionMode", "chatId": CHAT_ID, "mode": "full-access"}),
    )
    .await?;

    // Subscribe before queueing: terminal events are live-only (ADR-0019).
    let mut turn_events = watch(&engine, methods::WATCH_TURN_TERMINAL_EVENTS, json!({})).await?;
    let mut queue = watch(
        &engine,
        methods::WATCH_MESSAGE_QUEUE,
        json!({"chatId": CHAT_ID}),
    )
    .await?;
    let started = Instant::now();
    call(
        &engine,
        methods::QUEUE_COMMAND,
        json!({
            "chatId": CHAT_ID,
            "command": {
                "kind": "run",
                "messageId": MESSAGE_ID,
                "request": {
                    "prompt": args.prompt,
                    "provider": provider,
                    "model": args.model,
                    "reasoning": args.reasoning,
                    "modelOptions": {},
                    "cwd": args.cwd.to_string_lossy(),
                    "permissionMode": "full-access"
                }
            }
        }),
    )
    .await?;

    // The Turn's terminal event is the only completion signal. The queue
    // watch catches the prompt being rejected before any Turn starts
    // (unknown model, bad params), which never produces a terminal event.
    let deadline = tokio::time::sleep(args.timeout);
    tokio::pin!(deadline);
    let (status, reason) = loop {
        tokio::select! {
            event = turn_events.next() => {
                let event = event.ok_or_else(|| anyhow!("turn event watch ended"))?;
                if event["chatId"] == CHAT_ID && event["messageId"] == MESSAGE_ID {
                    break (
                        event["outcome"].as_str().unwrap_or("unknown").to_owned(),
                        event["internalReason"].as_str().map(str::to_owned),
                    );
                }
            }
            frame = queue.next() => {
                let frame = frame.ok_or_else(|| anyhow!("queue watch ended"))?;
                if let Some(error) = queue_error(&frame) {
                    break ("rejected".to_owned(), Some(error));
                }
            }
            () = &mut deadline => break ("timeout".to_owned(), None),
        }
    };
    let secs = started.elapsed().as_secs_f64();

    // The ledger settles when the queue completes; give it a moment to land.
    // A rejected prompt made no model call, so there is nothing to wait for.
    let mut usage_watch = watch(
        &engine,
        methods::WATCH_CHAT_USAGE,
        json!({"chatId": CHAT_ID}),
    )
    .await?;
    let mut usage = Value::Null;
    let wait = if status == "rejected" { 0 } else { 5 };
    let deadline = Instant::now() + Duration::from_secs(wait);
    while let Ok(Some(frame)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        usage_watch.next(),
    )
    .await
    {
        let settled = frame["recordCount"].as_u64().unwrap_or(0) > 0;
        usage = frame;
        if settled {
            break;
        }
    }

    let summary = json!({
        "status": status,
        "reason": reason,
        "commit": COMMIT,
        "model": args.model,
        "secs": (secs * 10.0).round() / 10.0,
        "usage": {
            "totalTokens": usage["gross"],
            "modelRequestCount": usage["recordCount"],
            "byKind": usage["byKind"],
        },
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    // Failed, rejected, and timed-out Turns are results, not runner
    // failures; still exit nonzero so a caller can tell them apart.
    std::process::exit(if status == "succeeded" { 0 } else { 1 });
}
