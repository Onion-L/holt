//! Workflows (ADR-workflows-run-scripts-in-the-background-and-report-by-continuation):
//! a script the parent model writes, run in the background, orchestrating
//! subagents through `agent()` / `parallel()` / `pipeline()`.
//!
//! The `Workflow` call returns at once. The run lives past its Turn: each
//! `agent()` becomes a display-only spawn chip under the call
//! (`{callId}#item-{n}`, the fan-out shape) and runs through the same
//! child path as `Agent`. When the script returns, its value is written to
//! the run directory and handed back as an engine-queued Turn whose entry
//! draws as a result row. Stopping a run, deleting its chat, or quitting
//! Holt cancels it without a report; the journal keeps every finished
//! agent for a `resume`.

pub(crate) mod journal;
pub(crate) mod plan;
pub(crate) mod script;

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use holt_core::{
    agent::types::{AgentTool, AgentToolResult},
    ai::types::{BlockContent, TextContent},
};
use holt_doc::{MessagePart, SubagentStatus};
use holt_proto::WorkflowPlan;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::subagents::{self, ChildTask, Delegation};
use journal::{Journal, Record};
use plan::PlanTracker;
use script::{AgentRequest, ScriptEvent};

const NAME_CHARS: usize = 80;
const LABEL_CHARS: usize = 80;
const LOG_TAIL: usize = 20;
/// `log()` keeps only the newest lines, each clipped, so a looping script
/// cannot grow the engine's memory or the failure report without bound.
const LOG_LINES: usize = 1000;
const LOG_LINE_CHARS: usize = 500;
const ERROR_CHARS: usize = 4000;

/// A finished run's report, on its way to the chat's queue.
pub(crate) struct Delivery {
    pub(crate) chat_id: String,
    pub(crate) name: String,
    pub(crate) failed: bool,
    pub(crate) prompt: String,
}

/// The live runs, by `(chatId, callId)` — the `Workflow` call's id is the
/// run's handle in the transcript and on `StopWorkflow`.
#[derive(Default)]
pub(crate) struct Workflows {
    runs: Mutex<HashMap<(String, String), CancellationToken>>,
    deliveries: OnceLock<mpsc::UnboundedSender<Delivery>>,
}

impl Workflows {
    pub(crate) fn set_delivery(&self, tx: mpsc::UnboundedSender<Delivery>) {
        let _ = self.deliveries.set(tx);
    }

    fn register(&self, chat_id: &str, call_id: &str, cancel: CancellationToken) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((chat_id.to_owned(), call_id.to_owned()), cancel);
    }

    fn unregister(&self, chat_id: &str, call_id: &str) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(chat_id.to_owned(), call_id.to_owned()));
    }

    /// Stop one run; `false` when no such run is live.
    pub(crate) fn stop(&self, chat_id: &str, call_id: &str) -> bool {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(&(chat_id.to_owned(), call_id.to_owned())) {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub(crate) fn stop_chat(&self, chat_id: &str) {
        for ((chat, _), cancel) in self.runs.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if chat == chat_id {
                cancel.cancel();
            }
        }
    }

    pub(crate) fn stop_all(&self) {
        for cancel in self.runs.lock().unwrap_or_else(|e| e.into_inner()).values() {
            cancel.cancel();
        }
    }

    fn deliver(&self, delivery: Delivery) {
        if let Some(tx) = self.deliveries.get() {
            let _ = tx.send(delivery);
        }
    }
}

/// The API alone leaves models writing the shallowest shape that works
/// (fan out, then summarize). The design section names the stages that make
/// a run trustworthy, and the example shows them composed.
const DESCRIPTION: &str = "Run a JavaScript workflow in the background that orchestrates many subagents — for work wider or longer than a few Agent calls: audits across many files, find → verify → fix stages, research cross-checked across sources. The call returns at once; when the script finishes, its return value arrives as a new message in this chat, so continue other work or end your turn.

Plan. Every call declares its `plan` first: `{steps: [{id, title, after?, agents?, loop?}]}`. The user sees this graph before any agent runs, and the run must follow it. `after` lists the steps this one depends on; `agents` is how many agents a round of it starts, when known; `loop: {to, max}` on a step sends work back to the upstream step `to`, at most `max` more rounds (review → revise). Every `agent()` names its `step`. An agent whose step is undeclared, whose `after` steps have no finished agent yet, or that runs a loop past `max` fails the whole run — so declare the graph the script really walks.

API. The script is an async function body (top-level await; `return` the result) with: \
`agent(prompt, {step, label, role: 'explorer'|'worker', schema})` → the child's final text, the object matching `schema` (a JSON Schema of type object), or null if it failed; any other option is an error; \
`parallel([() => agent(...), ...])` runs thunks concurrently (a failure is null); \
`pipeline(items, stage1, stage2, ...)` runs each item through the stages without waiting for other items (`stage(previous, item, index)`; a null result stops that item); \
`log(...)`; `args` is this call's `args`. \
Children start with fresh context — put everything they need in the prompt, and give workers non-overlapping files. Keep intermediate results in variables and return only what you need. The script itself has no filesystem, shell, imports, Date.now(), or Math.random(): agents do the work. Eight agents run at once across Holt, at most 1000 per run.

Design. Split the work by responsibility, not only by input: each step has one job and agents whose prompts are written for that job. Title each step after what it does. Pick the steps the task needs:
- Gather: parallel explorers, one per source, area, or question, returning `schema` objects so later steps can filter and count.
- Verify: an agent that did not produce the material re-checks it (re-reads the file, re-fetches the source, reproduces the bug) and marks each claim confirmed or not. Verify in batches — one verifier per gathered result, judging all of its claims — never one agent per claim. Drop or flag what fails; never pass unverified claims on as fact.
- Produce: a writer or fixer that works only from verified material.
- Review: one or more independent reviewers who did not take part read the output against the original request and return concrete problems.
- Revise: the producer fixes what the review found, and the review runs again, until no blocking problem remains or the loop's `max` (usually 2) is hit.
One gather-then-summarize pass is right only for quick lookups; for research, audits, and changes the user will rely on, add verification and review.

Example (research) plan:
{\"steps\": [{\"id\": \"research\", \"title\": \"Research\"}, {\"id\": \"verify\", \"title\": \"Verify\", \"after\": [\"research\"]}, {\"id\": \"write\", \"title\": \"Write\", \"after\": [\"verify\"], \"agents\": 1}, {\"id\": \"review\", \"title\": \"Review\", \"after\": [\"write\"], \"agents\": 2}, {\"id\": \"revise\", \"title\": \"Revise\", \"after\": [\"review\"], \"agents\": 1, \"loop\": {\"to\": \"review\", \"max\": 2}}]}

Example script:
const claims = {type:'object', properties:{claims:{type:'array', items:{type:'object', properties:{claim:{type:'string'}, source:{type:'string'}}, required:['claim','source']}}}, required:['claims']};
const checked = {type:'object', properties:{claims:{type:'array', items:{type:'object', properties:{claim:{type:'string'}, source:{type:'string'}, confirmed:{type:'boolean'}, note:{type:'string'}}, required:['claim','source','confirmed']}}}, required:['claims']};
const review = {type:'object', properties:{problems:{type:'array', items:{type:'string'}}}, required:['problems']};
const verified = await pipeline(args.topics,
  t => agent(`Research ${t}. Every claim needs a source URL.`, {step: 'research', label: t, schema: claims}),
  (found, t) => agent(`Fetch each source and mark whether it supports its claim:\\n${JSON.stringify(found.claims)}`, {step: 'verify', label: `verify:${t}`, schema: checked}));
const facts = verified.filter(Boolean).flatMap(v => v.claims.filter(c => c.confirmed));
let report = await agent(`Write the report from these verified facts only:\\n${JSON.stringify(facts)}`, {step: 'write', label: 'writer'});
const reviewOnce = async () => (await parallel(['accuracy', 'clarity'].map(lens => () => agent(`Review this report for ${lens}; list blocking problems only:\\n${report}`, {step: 'review', label: `reviewer:${lens}`, schema: review})))).filter(Boolean).flatMap(r => r.problems);
let problems = await reviewOnce();
for (let round = 0; problems.length && round < 2; round++) {
  report = await agent(`Revise the report to fix:\\n${problems.join('\\n')}\\n\\nReport:\\n${report}`, {step: 'revise', label: 'writer'});
  problems = await reviewOnce();
}
return {report, facts: facts.length, open: problems};

To rerun after a failure or an edit, pass `resume` with the run id (the saved plan and script rerun unless you pass new ones): agents that already finished with an identical prompt return their saved results.";

pub(crate) fn tool(delegation: Delegation) -> AgentTool {
    AgentTool {
        name: "Workflow".into(),
        label: "Workflow".into(),
        description: DESCRIPTION.into(),
        parameters: serde_json::json!({
            "type": "object", "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": NAME_CHARS, "description": "Short title shown in the transcript."},
                "plan": {
                    "type": "object",
                    "description": "The run's step graph, drawn before it starts and enforced while it runs. Required unless resuming.",
                    "properties": {
                        "steps": {"type": "array", "minItems": 1, "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "Referenced by agent({step})."},
                                "title": {"type": "string", "description": "What the step does, shown on the graph."},
                                "after": {"type": "array", "items": {"type": "string"}, "description": "Step ids that must have a finished agent first."},
                                "agents": {"type": "integer", "minimum": 1, "description": "Agents per round, when known."},
                                "loop": {"type": "object", "properties": {"to": {"type": "string"}, "max": {"type": "integer", "minimum": 1}}, "required": ["to", "max"], "additionalProperties": false, "description": "Send work back to upstream step `to`, at most `max` more rounds."}
                            },
                            "required": ["id", "title"], "additionalProperties": false
                        }}
                    },
                    "required": ["steps"], "additionalProperties": false
                },
                "script": {"type": "string", "description": "The workflow body. Optional with `resume` (the saved script reruns)."},
                "args": {"description": "Any JSON value, readable in the script as `args`."},
                "resume": {"type": "string", "description": "A previous run id from this chat to relaunch with its saved results."}
            }, "required": ["name"], "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |id, args, _, _| {
            let delegation = delegation.clone();
            let id = id.to_owned();
            let args = args.clone();
            Box::pin(async move { launch(delegation, id, args) })
        }),
    }
}

fn workflows_dir(d: &Delegation) -> PathBuf {
    subagents::directory(&d.runtime.data_dir, &d.parent.chat_id).join("workflows")
}

fn launch(
    d: Delegation,
    call_id: String,
    args: serde_json::Value,
) -> Result<AgentToolResult, String> {
    let chat_id = d.parent.chat_id.clone();
    if !crate::store::id_is_path_safe(&chat_id) || chat_id.contains("--sub--") {
        return Err("Workflows run only from a main chat".into());
    }
    let name: String = args["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or("name is required")?
        .chars()
        .take(NAME_CHARS)
        .collect();
    let previous = match args.get("resume").and_then(|r| r.as_str()) {
        Some(run) => {
            uuid::Uuid::parse_str(run)
                .map_err(|_| format!("No workflow run {run} in this chat"))?;
            let dir = workflows_dir(&d).join(run);
            if !dir.is_dir() {
                return Err(format!("No workflow run {run} in this chat"));
            }
            Some(dir)
        }
        None => None,
    };
    let script = match args["script"].as_str().filter(|s| !s.trim().is_empty()) {
        Some(script) => script.to_owned(),
        None => match &previous {
            Some(dir) => std::fs::read_to_string(dir.join("script.js"))
                .map_err(|e| format!("Could not read the saved script: {e}"))?,
            None => return Err("script is required".into()),
        },
    };
    let plan: WorkflowPlan = match args.get("plan").filter(|p| !p.is_null()) {
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|e| format!("plan is malformed: {e}"))?
        }
        None => match &previous {
            Some(dir) => std::fs::read_to_string(dir.join("plan.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .ok_or("the resumed run has no saved plan; pass plan")?,
            None => return Err("plan is required".into()),
        },
    };
    plan::validate(&plan)?;
    let run_args = match args.get("args") {
        Some(value) => value.clone(),
        None => previous
            .as_ref()
            .and_then(|dir| std::fs::read_to_string(dir.join("args.json")).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(serde_json::Value::Null),
    };
    script::check_syntax(&script)?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let run_dir = workflows_dir(&d).join(&run_id);
    let journal = Journal::open(&run_dir, previous.as_deref()).map_err(|e| e.to_string())?;
    subagents::write_result(&run_dir.join("script.js"), &script).map_err(|e| e.to_string())?;
    let args_text = serde_json::to_string_pretty(&run_args).map_err(|e| e.to_string())?;
    subagents::write_result(&run_dir.join("args.json"), &args_text).map_err(|e| e.to_string())?;
    let plan_text = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
    subagents::write_result(&run_dir.join("plan.json"), &plan_text).map_err(|e| e.to_string())?;

    // The run outlives this Turn: its own token (Stop and Steer leave it
    // alone), and no Turn change attribution — a background worker's edits
    // land after the Turn's set is frozen.
    let cancel = CancellationToken::new();
    let d = Delegation {
        cancel: cancel.clone(),
        attribution: None,
        ..d
    };
    d.runtime
        .workflows
        .register(&chat_id, &call_id, cancel.clone());
    stamp_container(
        &d,
        &call_id,
        Some((&name, &plan)),
        Some(SubagentStatus::Running),
        None,
    );
    // The plan is checked and drawn, and nothing has run yet: an approval
    // of the plan (by the user or a reviewing agent) would gate here.
    tokio::spawn(drive(
        d,
        Run {
            container: call_id,
            name: name.clone(),
            run_id: run_id.clone(),
            run_dir,
            script,
            args: run_args,
            plan,
            journal,
            cancel,
        },
    ));
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: format!(
                "Workflow \"{name}\" started in the background (run {run_id}). Its result arrives as a new message in this chat when it finishes; continue with other work or end your turn."
            ),
            ..Default::default()
        })],
        details: serde_json::json!({ "workflowRun": run_id }),
        ..Default::default()
    })
}

/// The `Workflow` call's own chip carries the run: its title and plan,
/// lifecycle, and the tokens its children spent so far.
fn stamp_container(
    d: &Delegation,
    container: &str,
    title: Option<(&str, &WorkflowPlan)>,
    status: Option<SubagentStatus>,
    tokens: Option<u64>,
) {
    subagents::edit_parent_parts(d, |list| {
        for part in list.iter_mut() {
            if let MessagePart::Tool {
                id,
                call,
                subagent_status,
                subagent_usage,
                ..
            } = part
                && id == container
            {
                if let Some((name, plan)) = title {
                    *call = holt_proto::ToolCall::Unknown {
                        name: format!("Workflow: {name}"),
                        input: Some(serde_json::json!({ "plan": plan })),
                    };
                }
                if let Some(status) = status {
                    *subagent_status = Some(status);
                }
                if tokens.is_some() {
                    *subagent_usage = tokens;
                }
            }
        }
    });
}

struct Run {
    container: String,
    name: String,
    run_id: String,
    run_dir: PathBuf,
    script: String,
    args: serde_json::Value,
    plan: WorkflowPlan,
    journal: Journal,
    cancel: CancellationToken,
}

async fn drive(d: Delegation, run: Run) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let tracker = Arc::new(Mutex::new(PlanTracker::new(run.plan.clone())));
    let mut done = script::spawn(
        run.script.clone(),
        run.args.clone(),
        tracker.clone(),
        tx,
        run.cancel.clone(),
    );
    let journal = Arc::new(Mutex::new(run.journal));
    let mut children = tokio::task::JoinSet::new();
    let mut next_item = 0usize;
    let mut tokens = 0u64;
    let mut logs: VecDeque<String> = VecDeque::new();
    let mut dropped_logs = 0usize;
    let mut events_open = true;
    let outcome = loop {
        tokio::select! {
            event = rx.recv(), if events_open => match event {
                Some(ScriptEvent::Agent(request)) => {
                    let part_id = format!("{}#item-{next_item}", run.container);
                    next_item += 1;
                    children.spawn(agent(
                        d.clone(),
                        run.container.clone(),
                        part_id,
                        request,
                        journal.clone(),
                        tracker.clone(),
                    ));
                }
                Some(ScriptEvent::Log(line)) => {
                    if logs.len() == LOG_LINES {
                        logs.pop_front();
                        dropped_logs += 1;
                    }
                    logs.push_back(clip(&line, LOG_LINE_CHARS));
                }
                None => events_open = false,
            },
            Some(joined) = children.join_next(), if !children.is_empty() => {
                tokens += joined.unwrap_or(0);
                stamp_container(&d, &run.container, None, None, Some(tokens));
            }
            result = &mut done => break result.unwrap_or_else(|_| Err("workflow host exited".into())),
        }
    };
    // Read before the sweep below: only a stop from outside (StopWorkflow,
    // chat deletion, shutdown) withholds the report.
    let stopped = run.cancel.is_cancelled();
    // Agents the script started but never awaited end with the run.
    run.cancel.cancel();
    while let Some(joined) = children.join_next().await {
        tokens += joined.unwrap_or(0);
    }
    let result_path = run.run_dir.join("result.json");
    let written = {
        // Chat deletion removes the run directory under this lock; writing
        // after it would recreate the directory.
        let _persistence = d
            .parent
            .persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if d.parent.is_removed() {
            Err("Parent chat was removed".to_owned())
        } else {
            if !logs.is_empty() {
                let mut text = String::new();
                if dropped_logs > 0 {
                    text.push_str(&format!("[{dropped_logs} earlier lines dropped]\n"));
                }
                for line in &logs {
                    text.push_str(line);
                    text.push('\n');
                }
                let _ = subagents::write_result(&run.run_dir.join("log.txt"), &text);
            }
            serde_json::to_string_pretty(&match &outcome {
                Ok(value) => value.clone(),
                Err(error) => serde_json::json!({ "error": error }),
            })
            .map_err(|e| e.to_string())
            .and_then(|text| {
                subagents::write_result(&result_path, &text).map_err(|e| e.to_string())?;
                Ok(text)
            })
        }
    };
    let failed = outcome.is_err() || written.is_err();
    stamp_container(
        &d,
        &run.container,
        None,
        Some(if failed {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Done
        }),
        Some(tokens),
    );
    d.runtime
        .workflows
        .unregister(&d.parent.chat_id, &run.container);
    if stopped || d.parent.is_removed() {
        return;
    }
    let prompt = match (&outcome, &written) {
        (Ok(_), Ok(text)) => {
            let path = std::fs::canonicalize(&result_path).unwrap_or(result_path);
            let body = subagents::bounded_result(text, &path).unwrap_or_else(|e| e);
            format!(
                "Workflow \"{}\" (run {}) finished. Its return value (also in {}):\n\n{body}",
                run.name,
                run.run_id,
                crate::tools::addressed_canonical(&path)
            )
        }
        _ => {
            let error = match (&outcome, &written) {
                (Err(error), _) | (_, Err(error)) => clip(error, ERROR_CHARS),
                _ => unreachable!(),
            };
            let tail = logs
                .iter()
                .skip(logs.len().saturating_sub(LOG_TAIL))
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n");
            let log = if tail.is_empty() {
                String::new()
            } else {
                format!("\n\nRecent log:\n{tail}")
            };
            format!(
                "Workflow \"{}\" (run {}) failed: {error}{log}\n\nTo rerun it, call Workflow with resume: \"{}\" (and an edited script if needed); agents that already finished return their saved results.",
                run.name, run.run_id, run.run_id
            )
        }
    };
    d.runtime.workflows.deliver(Delivery {
        chat_id: d.parent.chat_id.clone(),
        name: run.name,
        failed,
        prompt,
    });
}

/// One `agent()` call: its chip, then a cached result or a real child.
/// Returns the tokens it spent (a cached result spent them earlier). The
/// step settles in the plan before the script hears back, so the script's
/// next `agent()` already sees it finished.
/// `text` cut to `max` characters, marked when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

async fn agent(
    d: Delegation,
    container: String,
    part_id: String,
    request: AgentRequest,
    journal: Arc<Mutex<Journal>>,
    tracker: Arc<Mutex<PlanTracker>>,
) -> u64 {
    let AgentRequest {
        prompt,
        options,
        reply,
    } = request;
    let step = options.step.clone().unwrap_or_default();
    let reply = move |value: Option<serde_json::Value>| {
        tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settle(&step);
        let _ = reply.send(value);
    };
    let label = options
        .label
        .clone()
        .map(|label| label.trim().to_owned())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| {
            prompt
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_owned()
        });
    let label: String = label.chars().take(LABEL_CHARS).collect();
    subagents::insert_display_parts(
        &d,
        &container,
        vec![subagents::display_part(
            &part_id,
            &label,
            options.step.as_deref(),
        )],
    );
    let (key, cached) = {
        let mut journal = journal.lock().unwrap_or_else(|e| e.into_inner());
        let key = journal.key(&options.role, options.schema.as_ref(), &prompt);
        let cached = journal.cached(&key).cloned();
        (key, cached)
    };
    if let Some(record) = cached {
        subagents::settle_display_part(
            &d,
            &part_id,
            false,
            Some(&record.subagent),
            Some(record.tokens),
        );
        reply(Some(record.result));
        return 0;
    }
    let outcome = subagents::run_child(
        &d,
        ChildTask {
            part_id: part_id.clone(),
            role: options.role,
            label,
            prompt,
            schema: options.schema,
        },
    )
    .await;
    subagents::settle_outcome(&d, &part_id, &outcome);
    let Ok(outcome) = outcome else {
        reply(None);
        return 0;
    };
    let tokens = crate::usage::gross_tokens(&outcome.usage);
    if !outcome.ok {
        reply(None);
        return tokens;
    }
    let value = outcome
        .structured
        .unwrap_or(serde_json::Value::String(outcome.text));
    let _ = journal
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(Record {
            key,
            subagent: outcome.doc_id,
            result: value.clone(),
            tokens,
        });
    reply(Some(value));
    tokens
}

impl crate::EngineService {
    /// Start the task that turns finished runs into queued report Turns.
    pub(crate) fn spawn_workflow_delivery(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.runtime.workflows.set_delivery(tx);
        let service = self.clone();
        handle.spawn(async move {
            while let Some(delivery) = rx.recv().await {
                if let Err(error) = service.deliver_workflow(delivery) {
                    tracing::warn!(error = %error, "workflow report could not be queued");
                }
            }
        });
    }

    /// Queue a finished run's report as an ordinary run under the chat's
    /// captured settings — the goal continuation's path: a paused queue
    /// keeps the row until the user continues it.
    fn deliver_workflow(&self, delivery: Delivery) -> Result<(), holt_rpc::RpcError> {
        use holt_rpc::RpcError;
        // The runtime Arc before the registry row. A delete drops the row
        // first and the live runtime (then the queue file) only after, so a
        // row lookup that follows the Arc can never miss a delete that
        // already finished; in the other order, a runtime rebuilt after the
        // drop carried `removed = false` and the enqueue resurrected
        // queues/{chatId}.json for a chat the user deleted.
        let chat = self.runtime.chat(&delivery.chat_id);
        let request = {
            let chats = self.runtime.chats.read().unwrap_or_else(|e| e.into_inner());
            let row = chats
                .iter()
                .find(|row| row.id == delivery.chat_id)
                .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
            let config = row.config.clone().ok_or_else(|| {
                RpcError::Failed("the chat has no captured model settings".into())
            })?;
            let cwd = row
                .cwd
                .clone()
                .ok_or_else(|| RpcError::Failed("the chat has no working directory".into()))?;
            Self::queued_run_request(&config, &delivery.prompt, cwd)
        };
        let paused = {
            let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            if chat.is_removed() {
                return Err(RpcError::Failed("chat was deleted".into()));
            }
            queue.enqueue_workflow_result(
                request,
                uuid::Uuid::new_v4().to_string(),
                holt_proto::WorkflowResultMark {
                    name: delivery.name,
                    failed: delivery.failed,
                },
            )?;
            queue.paused()
        };
        if !paused {
            self.kick_queue(chat);
        }
        Ok(())
    }

    /// `StopWorkflow` (`{chatId, callId}`): cancel a live run. Its agents
    /// stop, no report is queued, and the journal stays for a `resume`.
    pub(crate) fn stop_workflow(
        &self,
        params: serde_json::Value,
    ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
        let chat_id = crate::rpc::required_string(&params, "chatId")?;
        let call_id = crate::rpc::required_string(&params, "callId")?;
        let stopped = self.runtime.workflows.stop(chat_id, call_id);
        holt_rpc::RpcReply::value(&serde_json::json!({ "stopped": stopped }))
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::Delivery;
    use holt_proto::{Chat, ChatConfig, PermissionMode, ProviderId};

    fn assembled(dir: &std::path::Path) -> crate::LocalEngine {
        crate::LocalEngine::assemble(&crate::EngineConfig {
            data_dir: dir.to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
            clock: None,
            exec_shell: None,
        })
        .unwrap()
    }

    fn chat_row(id: &str, cwd: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            cwd: Some(cwd.into()),
            config: Some(ChatConfig {
                provider: ProviderId("test".into()),
                model: "model".into(),
                reasoning: None,
                model_options: serde_json::Map::new(),
                permission_mode: PermissionMode::default(),
                scope: holt_proto::ChatScope::Normal,
            }),
            created_at: chrono::Utc::now(),
            title: None,
            title_source: holt_proto::TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            pinned: false,
            branch: None,
            checkout_id: None,
            source_context: None,
            last_message_preview: None,
            last_message_at: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            provider_mode: false,
            worktree: None,
            routine_run: None,
            goal: None,
        }
    }

    fn delivery(chat_id: &str) -> Delivery {
        Delivery {
            chat_id: chat_id.into(),
            name: "run".into(),
            failed: false,
            prompt: "report".into(),
        }
    }

    /// The delete-race end state: the workflow finished, the user deleted
    /// the chat, and the delivery lands only now. `runtime.chat()` rebuilds
    /// a `removed = false` runtime here, so the refusal must come from the
    /// registry row — the queue file must stay deleted.
    #[tokio::test]
    async fn a_delivery_landing_after_the_delete_does_not_resurrect_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let engine = assembled(dir.path());
        let runtime = engine.service.runtime.clone();
        runtime
            .chats
            .write()
            .unwrap()
            .push(chat_row("chat-1", "/tmp"));
        // The chat was open while its workflow ran.
        drop(runtime.chat("chat-1"));
        // deleteChat's steps, in order: the registry row, then the runtime
        // and its records.
        runtime
            .chats
            .write()
            .unwrap()
            .retain(|row| row.id != "chat-1");
        runtime.remove_chat("chat-1");

        let error = engine
            .service
            .deliver_workflow(delivery("chat-1"))
            .unwrap_err();
        assert!(error.to_string().contains("unknown chat"), "{error}");
        assert!(!dir.path().join("queues/chat-1.json").exists());
    }

    #[tokio::test]
    async fn a_delivery_for_a_live_chat_queues_the_marked_report() {
        let dir = tempfile::tempdir().unwrap();
        let engine = assembled(dir.path());
        let runtime = engine.service.runtime.clone();
        runtime
            .chats
            .write()
            .unwrap()
            .push(chat_row("chat-1", "/tmp"));
        let chat = runtime.chat("chat-1");
        // Parked, so the delivery only queues — no driver, no model call.
        chat.queue.lock().unwrap().pause(true).unwrap();

        engine.service.deliver_workflow(delivery("chat-1")).unwrap();
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("queues/chat-1.json")).unwrap())
                .unwrap();
        assert_eq!(record["pending"].as_array().unwrap().len(), 1);
        assert_eq!(record["pending"][0]["workflowResult"]["name"], "run");
        assert!(record["paused"].as_bool().unwrap());
    }
}
