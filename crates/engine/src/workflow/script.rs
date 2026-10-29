//! The workflow script host: one embedded QuickJS per run, on its own
//! thread. The script sees `agent()` / `parallel()` / `pipeline()` /
//! `phase()` / `log()` / `args` ([`PRELUDE`]); every `agent()` call leaves
//! the JS thread as an [`AgentRequest`] and resolves with whatever the
//! engine side replies. The host knows nothing of chats or children — the
//! caller owns what an agent request does.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt as _, Context, Function, Promise, Runtime,
    prelude::Async,
};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PRELUDE: &str = include_str!("prelude.js");
/// Runaway guard: a script loop can spawn without a model round in between,
/// so a run stops admitting agents past this many.
pub(crate) const MAX_AGENTS_PER_RUN: usize = 1000;
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// One `agent()` call's options as the script passed them (the prelude
/// fills `phase` from the current `phase()` and `role` with `explorer`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct AgentOptions {
    pub(crate) label: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) role: String,
    pub(crate) schema: Option<serde_json::Value>,
}

/// An `agent()` call leaving the script. The reply is the child's result —
/// its text, or its structured object — or `None` when the child failed or
/// was stopped (the script reads `null`).
pub(crate) struct AgentRequest {
    pub(crate) prompt: String,
    pub(crate) options: AgentOptions,
    pub(crate) reply: oneshot::Sender<Option<serde_json::Value>>,
}

pub(crate) enum ScriptEvent {
    Agent(AgentRequest),
    Log(String),
}

/// The script body wrapped so top-level `await` and `return` work and the
/// return value leaves as JSON (`undefined` reads as `null`).
fn wrap(script: &str) -> String {
    format!(
        "(async () => {{ const __holt_result = await (async () => {{\n{script}\n}})(); \
         return JSON.stringify(__holt_result === undefined ? null : __holt_result); }})()"
    )
}

/// Parse the script without running it, so a syntax error fails the
/// `Workflow` call itself instead of a run that never starts. Defining the
/// wrapper function compiles its body; nothing is called.
pub(crate) fn check_syntax(script: &str) -> Result<(), String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    context.with(|ctx| {
        ctx.eval::<(), _>(format!("(async () => {{\n{script}\n}});"))
            .catch(&ctx)
            .map_err(|e| format!("script does not parse: {e}"))
    })
}

/// Run `script` on a dedicated thread. Agent calls and log lines arrive on
/// `events`; the returned receiver yields the script's return value, or the
/// error that ended it. Cancelling `cancel` interrupts the script (a busy
/// loop included) and ends the run with an error.
pub(crate) fn spawn(
    script: String,
    args: serde_json::Value,
    events: mpsc::UnboundedSender<ScriptEvent>,
    cancel: CancellationToken,
) -> oneshot::Receiver<Result<serde_json::Value, String>> {
    let (done_tx, done_rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("holt-workflow".into())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err("workflow stopped".to_string()),
                        result = run(script, args, events, cancel.clone()) => result,
                    }
                }),
                Err(error) => Err(error.to_string()),
            };
            let _ = done_tx.send(result);
        });
    if let Err(error) = spawned {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Err(error.to_string()));
        return rx;
    }
    done_rx
}

async fn run(
    script: String,
    args: serde_json::Value,
    events: mpsc::UnboundedSender<ScriptEvent>,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let runtime = AsyncRuntime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let interrupted = interrupted.clone();
        let cancel = cancel.clone();
        runtime
            .set_interrupt_handler(Some(Box::new(move || {
                let stop = cancel.is_cancelled();
                if stop {
                    interrupted.store(true, Ordering::Release);
                }
                stop
            })))
            .await;
    }
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|e| e.to_string())?;
    let args = serde_json::to_string(&args).map_err(|e| e.to_string())?;
    let spawned = Arc::new(AtomicUsize::new(0));
    let output: Result<String, String> = context
        .async_with(async |ctx| {
            let globals = ctx.globals();
            let agent_events = events.clone();
            let agent = Function::new(
                ctx.clone(),
                Async(move |prompt: String, options: String| {
                    let events = agent_events.clone();
                    let spawned = spawned.clone();
                    async move {
                        if spawned.fetch_add(1, Ordering::AcqRel) >= MAX_AGENTS_PER_RUN {
                            return Err(rquickjs::Error::new_from_js_message(
                                "agent",
                                "result",
                                format!("a run may start at most {MAX_AGENTS_PER_RUN} agents"),
                            ));
                        }
                        let options: AgentOptions =
                            serde_json::from_str(&options).map_err(|e| {
                                rquickjs::Error::new_from_js_message(
                                    "options",
                                    "AgentOptions",
                                    e.to_string(),
                                )
                            })?;
                        let (reply, answer) = oneshot::channel();
                        let sent = events.send(ScriptEvent::Agent(AgentRequest {
                            prompt,
                            options,
                            reply,
                        }));
                        // A host that went away (run stopped) answers as a
                        // failed child: the script reads null.
                        let value = match sent {
                            Ok(()) => answer.await.ok().flatten(),
                            Err(_) => None,
                        };
                        Ok::<_, rquickjs::Error>(
                            serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
                        )
                    }
                }),
            )
            .map_err(|e| e.to_string())?;
            let log_events = events.clone();
            let log = Function::new(ctx.clone(), move |text: String| {
                let _ = log_events.send(ScriptEvent::Log(text));
            })
            .map_err(|e| e.to_string())?;
            globals
                .set("__holt_agent", agent)
                .map_err(|e| e.to_string())?;
            globals.set("__holt_log", log).map_err(|e| e.to_string())?;
            globals
                .set("__holt_args", args)
                .map_err(|e| e.to_string())?;
            ctx.eval::<(), _>(PRELUDE)
                .catch(&ctx)
                .map_err(|e| format!("workflow prelude failed: {e}"))?;
            let promise: Promise = ctx
                .eval(wrap(&script))
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            promise
                .into_future::<String>()
                .await
                .catch(&ctx)
                .map_err(|e| e.to_string())
        })
        .await;
    if interrupted.load(Ordering::Acquire) {
        return Err("workflow stopped".into());
    }
    let output = output?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Drive a script against a fake host: `answer` maps a prompt to its
    /// reply (absent → the child failed). Returns the result, the requests
    /// in arrival order, and the log lines.
    async fn drive(
        script: &str,
        args: serde_json::Value,
        answer: impl Fn(&str, &AgentOptions) -> Option<serde_json::Value> + Send + 'static,
    ) -> (
        Result<serde_json::Value, String>,
        Vec<(String, AgentOptions)>,
        Vec<String>,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let done = spawn(script.into(), args, tx, CancellationToken::new());
        let mut requests = Vec::new();
        let mut logs = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                ScriptEvent::Agent(request) => {
                    let reply = answer(&request.prompt, &request.options);
                    requests.push((request.prompt, request.options));
                    let _ = request.reply.send(reply);
                }
                ScriptEvent::Log(line) => logs.push(line),
            }
        }
        (done.await.unwrap(), requests, logs)
    }

    fn echo(prompt: &str, _: &AgentOptions) -> Option<serde_json::Value> {
        Some(serde_json::Value::String(format!("did {prompt}")))
    }

    #[tokio::test]
    async fn returns_the_script_value_as_json() {
        let (result, requests, _) = drive(
            "const a = await agent('one'); return { a, n: args.n };",
            serde_json::json!({"n": 3}),
            echo,
        )
        .await;
        assert_eq!(result.unwrap(), serde_json::json!({"a": "did one", "n": 3}));
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1.role, "explorer");
    }

    #[tokio::test]
    async fn no_return_reads_as_null() {
        let (result, _, _) = drive("await agent('x');", serde_json::Value::Null, echo).await;
        assert_eq!(result.unwrap(), serde_json::Value::Null);
    }

    #[tokio::test]
    async fn parallel_starts_in_order_and_a_failure_is_null() {
        let (result, requests, logs) = drive(
            "return await parallel(['a', 'b', 'c'].map(x => () => agent(x)));",
            serde_json::Value::Null,
            |prompt, _| (prompt != "b").then(|| serde_json::json!(prompt)),
        )
        .await;
        assert_eq!(result.unwrap(), serde_json::json!(["a", null, "c"]));
        let prompts: Vec<_> = requests.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(prompts, ["a", "b", "c"]);
        assert!(
            logs.is_empty(),
            "a null child is not an exception: {logs:?}"
        );
    }

    #[tokio::test]
    async fn a_throwing_parallel_step_is_logged_and_null() {
        let (result, _, logs) = drive(
            "return await parallel([() => agent('a'), () => { throw new Error('boom'); }]);",
            serde_json::Value::Null,
            echo,
        )
        .await;
        assert_eq!(result.unwrap(), serde_json::json!(["did a", null]));
        assert_eq!(logs, ["step failed: boom"]);
    }

    #[tokio::test]
    async fn pipeline_items_do_not_wait_for_each_other() {
        // Item 0's first stage is held until item 1 reached its second
        // stage: a barrier between stages would deadlock this script.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let done = spawn(
            "return await pipeline(['x', 'y'], (v) => agent('find ' + v), (v) => agent('check ' + v));"
                .into(),
            serde_json::Value::Null,
            tx,
            CancellationToken::new(),
        );
        let mut held: Option<AgentRequest> = None;
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let ScriptEvent::Agent(request) = event else {
                continue;
            };
            seen.push(request.prompt.clone());
            if request.prompt == "find x" {
                held = Some(request);
                continue;
            }
            let reply = serde_json::json!(request.prompt.replace("find ", ""));
            let _ = request.reply.send(Some(reply));
            if seen.contains(&"check y".to_string())
                && let Some(first) = held.take()
            {
                let _ = first.reply.send(Some(serde_json::json!("x")));
            }
        }
        assert_eq!(
            done.await.unwrap().unwrap(),
            serde_json::json!(["check x", "check y"])
        );
        let check_y = seen.iter().position(|p| p == "check y").unwrap();
        let check_x = seen.iter().position(|p| p == "check x").unwrap();
        assert!(check_y < check_x, "item 1 ran ahead: {seen:?}");
    }

    #[tokio::test]
    async fn options_and_phase_reach_the_host() {
        let (_, requests, _) = drive(
            "phase('Review'); await agent('a', { label: 'L', role: 'worker', schema: { type: 'object' } }); \
             await agent('b', { phase: 'Other' }); phase(null); await agent('c');",
            serde_json::Value::Null,
            |_, _| Some(serde_json::json!({"ok": true})),
        )
        .await;
        let options: HashMap<_, _> = requests.into_iter().collect();
        assert_eq!(
            options["a"],
            AgentOptions {
                label: Some("L".into()),
                phase: Some("Review".into()),
                role: "worker".into(),
                schema: Some(serde_json::json!({"type": "object"})),
            }
        );
        assert_eq!(options["b"].phase.as_deref(), Some("Other"));
        assert_eq!(options["c"].phase, None);
    }

    #[tokio::test]
    async fn unknown_agent_options_fail_the_run() {
        let (result, requests, _) = drive(
            "await agent('a', { output_schema: { type: 'object' } });",
            serde_json::Value::Null,
            echo,
        )
        .await;
        assert!(requests.is_empty());
        let error = result.unwrap_err();
        assert!(error.contains("no option \"output_schema\""), "{error}");
        assert!(error.contains("did you mean schema?"), "{error}");
    }

    #[tokio::test]
    async fn the_tool_description_example_runs() {
        let description = super::super::DESCRIPTION;
        let start = description.find("Example (research):\n").unwrap() + 20;
        let end = description.find("\n\nTo rerun").unwrap();
        let example = &description[start..end];
        let reviews = std::sync::atomic::AtomicUsize::new(0);
        let (result, requests, _) = drive(
            example,
            serde_json::json!({"topics": ["a", "b"]}),
            move |prompt, options| {
                Some(match options.label.as_deref() {
                    Some("verify") => serde_json::json!({"confirmed": !prompt.contains("bad")}),
                    Some("writer") => serde_json::json!("report"),
                    Some(label) if label.starts_with("reviewer") => {
                        // The first review round finds a problem; the second passes.
                        let n = reviews.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        serde_json::json!({"problems": if n < 2 { vec!["fix"] } else { vec![] }})
                    }
                    _ => serde_json::json!({"claims": [
                        {"claim": "good", "source": "u"},
                        {"claim": "bad", "source": "u"},
                    ]}),
                })
            },
        )
        .await;
        assert_eq!(
            result.unwrap(),
            serde_json::json!({"report": "report", "unverified": 2})
        );
        let phases: Vec<_> = requests
            .iter()
            .map(|(_, options)| options.phase.as_deref().unwrap())
            .collect();
        assert_eq!(
            phases,
            [
                "Research", "Research", "Verify", "Verify", "Verify", "Verify", "Write", "Review",
                "Review", "Revise", "Review", "Review",
            ]
        );
    }

    #[tokio::test]
    async fn clocks_and_randomness_throw() {
        for script in ["Date.now()", "Math.random()", "new Date()"] {
            let (result, _, _) = drive(script, serde_json::Value::Null, echo).await;
            let error = result.unwrap_err();
            assert!(
                error.contains("unavailable in workflow scripts"),
                "{script}: {error}"
            );
        }
        let (result, _, _) = drive(
            "return new Date(0).toISOString();",
            serde_json::Value::Null,
            echo,
        )
        .await;
        assert_eq!(
            result.unwrap(),
            serde_json::json!("1970-01-01T00:00:00.000Z")
        );
    }

    #[tokio::test]
    async fn the_agent_cap_rejects_past_the_limit() {
        let (result, requests, _) = drive(
            &format!(
                "for (let i = 0; i < {MAX_AGENTS_PER_RUN}; i++) await agent('a' + i); await agent('over');"
            ),
            serde_json::Value::Null,
            echo,
        )
        .await;
        assert_eq!(requests.len(), MAX_AGENTS_PER_RUN);
        assert!(result.unwrap_err().contains("at most 1000 agents"));
    }

    #[tokio::test]
    async fn oversized_fanouts_are_refused() {
        let (result, requests, _) = drive(
            "return await parallel(Array.from({ length: 4097 }, () => () => agent('x')));",
            serde_json::Value::Null,
            echo,
        )
        .await;
        assert!(requests.is_empty());
        assert!(result.unwrap_err().contains("at most 4096 items"));
    }

    #[tokio::test]
    async fn imports_and_errors_fail_the_run() {
        let (result, _, _) = drive("await import('fs');", serde_json::Value::Null, echo).await;
        assert!(result.is_err());
        let (result, _, _) = drive("throw new Error('nope');", serde_json::Value::Null, echo).await;
        assert!(result.unwrap_err().contains("nope"));
    }

    #[test]
    fn syntax_errors_are_caught_before_running() {
        assert!(check_syntax("return await agent('x');").is_ok());
        assert!(
            check_syntax("const = ;")
                .unwrap_err()
                .contains("does not parse")
        );
        // Parsing never executes: a top-level throw is not a syntax error.
        assert!(check_syntax("throw new Error('later');").is_ok());
    }

    #[tokio::test]
    async fn cancelling_interrupts_a_busy_script() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let done = spawn(
            "while (true) {}".into(),
            serde_json::Value::Null,
            tx,
            cancel.clone(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .expect("interrupted promptly")
            .unwrap();
        assert_eq!(result.unwrap_err(), "workflow stopped");
    }
}
