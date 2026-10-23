//! holt-runner — one headless Turn against the local engine, for eval harnesses and scripts.
//!
//! Assembles `LocalEngine` in a fresh data dir (seeded with the provider
//! catalog and credentials from the user's data dir), creates one
//! full-access chat, queues the prompt, waits for the Turn's terminal event,
//! and prints a JSON summary on stdout. No UI; the engine is driven only
//! through the RPC contract, exactly as the shell drives it. The transcript
//! stays under `<data-dir>/transcripts/` for the caller to read.
//!
//! `--resume` reopens the data dir of a run that was killed mid-Turn: the
//! engine relaunches the cut-off Turn as it assembles
//! (ADR-turns-resume-from-an-intent-journal), and the runner only waits for
//! it — the crash-injection eval's restart.

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
--data-dir <fresh dir> [--source-data-dir <dir>] [--reasoning <level>] [--timeout-sec <n>]
       holt-runner --resume --data-dir <dir of a killed run> [--timeout-sec <n>]";

/// A fresh run's Turn request; a resumed run's is already on record.
struct Fresh {
    cwd: PathBuf,
    prompt: String,
    model: String,
    source_data_dir: PathBuf,
    reasoning: Option<String>,
}

struct Args {
    data_dir: PathBuf,
    /// `None` with `--resume`.
    fresh: Option<Fresh>,
    timeout: Duration,
    /// Runs every agent command instead of the login shell — the crash eval
    /// forwards commands into a task's container this way.
    shell: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut cwd = None;
    let mut prompt_file = None;
    let mut model = None;
    let mut data_dir = None;
    let mut source_data_dir = None;
    let mut reasoning = None;
    let mut timeout_sec = 1800u64;
    let mut resume = false;
    let mut shell = None;
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
            "--resume" => resume = true,
            "--shell" => shell = Some(value()?),
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
    let data_dir = data_dir.ok_or_else(|| anyhow!("--data-dir is required\n{USAGE}"))?;
    let timeout = Duration::from_secs(timeout_sec);
    if resume {
        if cwd.is_some() || prompt_file.is_some() || model.is_some() || reasoning.is_some() {
            bail!("--resume takes the Turn from --data-dir; drop the request flags\n{USAGE}");
        }
        return Ok(Args {
            data_dir,
            fresh: None,
            timeout,
            shell,
        });
    }
    let prompt_file = prompt_file.ok_or_else(|| anyhow!("--prompt-file is required\n{USAGE}"))?;
    let source_data_dir = match source_data_dir {
        Some(dir) => dir,
        None => match std::env::var_os("HOLT_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()
                .context("could not locate the user's home directory")?
                .join(".holt"),
        },
    };
    Ok(Args {
        data_dir,
        fresh: Some(Fresh {
            cwd: cwd
                .ok_or_else(|| anyhow!("--cwd is required\n{USAGE}"))?
                .canonicalize()?,
            prompt: std::fs::read_to_string(&prompt_file)
                .with_context(|| format!("reading {}", prompt_file.display()))?,
            model: model.ok_or_else(|| anyhow!("--model is required\n{USAGE}"))?,
            source_data_dir,
            reasoning,
        }),
        timeout,
        shell,
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

/// Whether a queue frame shows the prompt neither running nor waiting.
fn queue_idle(frame: &Value) -> bool {
    let pending = frame["pending"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["messageId"] == MESSAGE_ID));
    frame["activeMessageId"] != MESSAGE_ID && !pending
}

fn history_entries(data_dir: &Path) -> Vec<Value> {
    let path = data_dir.join("history").join(format!("{CHAT_ID}.jsonl"));
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The Turn's outcome as its History ends, for a resumed Turn whose live
/// terminal event fired before the runner subscribed (terminal events are
/// live-only) or that the engine declined to relaunch.
fn history_outcome(entries: &[Value]) -> &'static str {
    let last = entries
        .iter()
        .rev()
        .find(|entry| entry["kind"] == "message")
        .map(|entry| &entry["entry"]);
    match last {
        Some(message) if message["role"] == "assistant" => match message["stopReason"].as_str() {
            Some("stop") => "succeeded",
            Some("error") => "failed",
            _ => "interrupted",
        },
        _ => "interrupted",
    }
}

/// What the run's History records across every process that worked on it —
/// the usage ledger books a Turn only when it settles, so a killed run's
/// model calls are in the History alone. Calls still streaming at the kill
/// left no message and are not counted.
fn history_usage(entries: &[Value]) -> Value {
    let mut requests = 0u64;
    let mut tokens = 0u64;
    let mut tool_calls = 0u64;
    let mut intents = 0u64;
    for entry in entries {
        match entry["kind"].as_str() {
            Some("message") if entry["entry"]["role"] == "assistant" => {
                let message = &entry["entry"];
                requests += 1;
                tokens += message["usage"]["totalTokens"].as_u64().unwrap_or(0);
                tool_calls += message["content"].as_array().map_or(0, |blocks| {
                    blocks.iter().filter(|b| b["type"] == "toolCall").count() as u64
                });
            }
            Some("toolIntent") => intents += 1,
            _ => {}
        }
    }
    json!({
        "totalTokens": tokens,
        "modelRequestCount": requests,
        "toolCalls": tool_calls,
        "toolIntents": intents,
    })
}

/// How long a resumed run waits for the live terminal event once the queue
/// shows the Turn settled, before reading the outcome from the History.
const SETTLE_GRACE: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args()?;
    match &args.fresh {
        Some(fresh) => {
            fresh
                .model
                .split_once('/')
                .ok_or_else(|| anyhow!("--model must be provider-qualified (provider/model)"))?;
            seed_data_dir(&fresh.source_data_dir, &args.data_dir)?;
        }
        None => {
            if history_entries(&args.data_dir).is_empty() {
                bail!(
                    "--resume: {} holds no run to resume",
                    args.data_dir.display()
                );
            }
        }
    }
    // A resumed run's Turn relaunches inside `assemble`.
    let engine = LocalEngine::assemble(&EngineConfig {
        data_dir: args.data_dir.clone(),
        personal_skills_dir: Some(args.data_dir.join("personal-skills")),
        stream_fn: None,
        search_backend_resolver: None,
        clock: None,
        exec_shell: args.shell.clone(),
    })
    .map_err(|error| anyhow!("assembling the engine: {error}"))?;

    if args.fresh.is_some() {
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
    }

    // Subscribe before queueing: terminal events are live-only (ADR-turn-completion-is-an-engine-owned-event).
    // A resumed Turn is already running, so its event can be missed; the
    // queue watch's current frame covers that.
    let mut turn_events = watch(&engine, methods::WATCH_TURN_TERMINAL_EVENTS, json!({})).await?;
    let mut queue = watch(
        &engine,
        methods::WATCH_MESSAGE_QUEUE,
        json!({"chatId": CHAT_ID}),
    )
    .await?;
    let started = Instant::now();
    if let Some(fresh) = &args.fresh {
        let (provider, _) = fresh.model.split_once('/').expect("checked above");
        call(
            &engine,
            methods::QUEUE_COMMAND,
            json!({
                "chatId": CHAT_ID,
                "command": {
                    "kind": "run",
                    "messageId": MESSAGE_ID,
                    "request": {
                        "prompt": fresh.prompt,
                        "provider": provider,
                        "model": fresh.model,
                        "reasoning": fresh.reasoning,
                        "modelOptions": {},
                        "cwd": fresh.cwd.to_string_lossy(),
                        "permissionMode": "full-access"
                    }
                }
            }),
        )
        .await?;
    }

    // The Turn's terminal event is the only completion signal. The queue
    // watch catches the prompt being rejected before any Turn starts
    // (unknown model, bad params), which never produces a terminal event,
    // and — resuming — the Turn settling unobserved.
    let deadline = tokio::time::sleep(args.timeout);
    tokio::pin!(deadline);
    let mut relaunched = None;
    let (status, reason, status_source) = loop {
        tokio::select! {
            event = turn_events.next() => {
                let event = event.ok_or_else(|| anyhow!("turn event watch ended"))?;
                if event["chatId"] == CHAT_ID && event["messageId"] == MESSAGE_ID {
                    break (
                        event["outcome"].as_str().unwrap_or("unknown").to_owned(),
                        event["internalReason"].as_str().map(str::to_owned),
                        "event",
                    );
                }
            }
            frame = queue.next() => {
                let frame = frame.ok_or_else(|| anyhow!("queue watch ended"))?;
                if args.fresh.is_some() {
                    if let Some(error) = queue_error(&frame) {
                        break ("rejected".to_owned(), Some(error), "queue");
                    }
                    continue;
                }
                let idle = queue_idle(&frame);
                // The first frame says whether the engine relaunched the Turn.
                relaunched.get_or_insert(!idle);
                if idle {
                    let grace = tokio::time::timeout(SETTLE_GRACE, async {
                        while let Some(event) = turn_events.next().await {
                            if event["chatId"] == CHAT_ID && event["messageId"] == MESSAGE_ID {
                                return Some(event);
                            }
                        }
                        None
                    })
                    .await;
                    break match grace {
                        Ok(Some(event)) => (
                            event["outcome"].as_str().unwrap_or("unknown").to_owned(),
                            event["internalReason"].as_str().map(str::to_owned),
                            "event",
                        ),
                        _ => (
                            history_outcome(&history_entries(&args.data_dir)).to_owned(),
                            None,
                            "history",
                        ),
                    };
                }
            }
            () = &mut deadline => break ("timeout".to_owned(), None, "timeout"),
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
        "statusSource": status_source,
        "reason": reason,
        "commit": COMMIT,
        "model": args.fresh.as_ref().map(|fresh| fresh.model.clone()),
        "resumed": args.fresh.is_none(),
        "relaunched": relaunched,
        "secs": (secs * 10.0).round() / 10.0,
        "usage": {
            "totalTokens": usage["gross"],
            "modelRequestCount": usage["recordCount"],
            "byKind": usage["byKind"],
        },
        "history": history_usage(&history_entries(&args.data_dir)),
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    // Failed, rejected, and timed-out Turns are results, not runner
    // failures; still exit nonzero so a caller can tell them apart.
    std::process::exit(if status == "succeeded" { 0 } else { 1 });
}
