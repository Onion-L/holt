//! Goal mode (ADR-0044): a chat-scoped objective the queue driver loops on.
//!
//! The goal lives on the chat row (`Chat::goal`); the verifier is one model
//! pass that runs after a settled Turn's card frame is on its way (after
//! `clear_final_signal`, inside the same driver iteration, so the
//! continuation it may enqueue can never race the next admission). The
//! verdict is judged from evidence — the History tail's tool calls and
//! results plus the Turn's frozen change set — never from the model's own
//! plans. A `CONTINUE` verdict enqueues an ordinary, visible queue item
//! flagged `goalContinuation`; deleting that item pauses the goal, and
//! `ClearGoal`/pausing sweeps every flagged row. `MAX_ITERATIONS` is the
//! primary backstop; the no-progress counter (a Succeeded Turn with no tool
//! call and no change set) is the secondary one.

use std::sync::Arc;

use chrono::Utc;
use holt_proto::{
    Chat, ChatGoalState, GoalRound, GoalRoundKind, GoalStatus, GoalStep, TurnChangeSet,
};
use holt_rpc::RpcError;
use pi_core::agent::types::AgentMessage;
use tokio_util::sync::CancellationToken;

use crate::EngineService;
use crate::agent::ChatRuntime;

/// The objective's length cap; longer objectives belong in the
/// conversation itself.
pub(crate) const MAX_GOAL_CHARS: usize = 4000;
/// Hard cap on enqueued continuations — the primary anti-spin backstop.
pub(crate) const MAX_ITERATIONS: u32 = 20;
/// Consecutive Succeeded Turns with no tool call and no change set before
/// the loop pauses itself.
pub(crate) const MAX_NO_PROGRESS: u32 = 3;
/// Consecutive verifier failures (provider error or garbled verdict) before
/// the loop pauses itself.
pub(crate) const MAX_EVAL_FAILURES: u32 = 3;
/// Byte cap on the History tail the verifier reads.
const EVIDENCE_TAIL_BYTES: usize = 6000;
/// Byte cap on one tool result's excerpt inside the evidence tail.
const TOOL_RESULT_BYTES: usize = 500;
/// How many change-set files the summary lists before "and N more".
const CHANGE_SET_LISTED: usize = 20;

/// The settled Turn's outcome, as the goal loop cares about it.
pub(crate) enum SettledTurn {
    Succeeded,
    Failed,
    Interrupted,
}

/// The verifier's one-line verdict (the prompt's reply protocol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GoalVerdict {
    Complete(String),
    Continue(String),
    Blocked(String),
}

/// The verifier's full reply: the verdict line plus the card-facing state
/// it maintains — the goal's short name and the current step list. Both are
/// optional: a reply without them leaves the card as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalCheckReport {
    pub(crate) verdict: GoalVerdict,
    pub(crate) name: Option<String>,
    pub(crate) steps: Vec<GoalStep>,
}

/// The card-facing caps: the name is a few words, the plan a handful of
/// short steps.
const MAX_GOAL_NAME_CHARS: usize = 60;
pub(crate) const MAX_GOAL_STEPS: usize = 7;
const MAX_STEP_CHARS: usize = 80;

/// Strict first-line parse, mirroring `gate::parse_review_reply`: anything
/// else is no verdict at all — the caller counts it as an evaluation
/// failure, never as a fabricated `Continue`.
pub(crate) fn parse_verdict(reply: &str) -> Option<GoalVerdict> {
    let first = reply.trim().lines().next().unwrap_or_default().trim();
    for (prefix, build) in [
        (
            "COMPLETE",
            GoalVerdict::Complete as fn(String) -> GoalVerdict,
        ),
        ("CONTINUE", GoalVerdict::Continue),
        ("BLOCKED", GoalVerdict::Blocked),
    ] {
        if let Some(rest) = first.strip_prefix(prefix) {
            // The keyword must end the word: "CONTINUED" is not a verdict.
            let rest = rest.trim_start();
            if !rest.is_empty() && !rest.starts_with(':') {
                continue;
            }
            let reason = rest.trim_start_matches(':').trim();
            // COMPLETE is the verdict that ends the loop and clears the
            // objective: it must cite its evidence. A bare COMPLETE is no
            // verdict at all — the caller counts an evaluation failure.
            if reason.is_empty() && prefix == "COMPLETE" {
                return None;
            }
            return Some(build(if reason.is_empty() {
                "no reason given".to_string()
            } else {
                // The reason lands in the chat as a one-line status row.
                reason.chars().take(200).collect()
            }));
        }
    }
    None
}

/// Parse the verifier's reply: the strict first-line verdict, then the
/// card-facing extras — a `NAME:` line and markdown task-list step lines
/// (`- [x] done` / `- [ ] pending`). The extras are lenient on purpose:
/// they paint the card, so a sloppy line is skipped, never fatal.
pub(crate) fn parse_check_report(reply: &str) -> Option<GoalCheckReport> {
    let verdict = parse_verdict(reply)?;
    let mut name = None;
    let mut steps = Vec::new();
    for line in reply.lines().skip(1) {
        let line = line.trim();
        if name.is_none()
            && let Some(rest) = line.strip_prefix("NAME:")
        {
            let cleaned = rest.trim();
            if !cleaned.is_empty() {
                name = Some(cleaned.chars().take(MAX_GOAL_NAME_CHARS).collect());
            }
            continue;
        }
        if line == "STEPS:" {
            continue;
        }
        let Some(body) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else {
            continue;
        };
        let (done, text) = if let Some(text) = body.strip_prefix("[x]") {
            (true, text)
        } else if let Some(text) = body.strip_prefix("[X]") {
            (true, text)
        } else if let Some(text) = body.strip_prefix("[ ]") {
            (false, text)
        } else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() && steps.len() < MAX_GOAL_STEPS {
            steps.push(GoalStep {
                text: text.chars().take(MAX_STEP_CHARS).collect(),
                done,
            });
        }
    }
    Some(GoalCheckReport {
        verdict,
        name,
        steps,
    })
}

/// Fold the verifier's card-facing state into the row: a fresh name or step
/// list REPLACES the old one; an absent one leaves it — a flaky reply must
/// not wipe the plan.
fn apply_card_state(goal: &mut ChatGoalState, name: Option<String>, steps: Vec<GoalStep>) {
    if let Some(name) = name {
        goal.name = Some(name);
    }
    if !steps.is_empty() {
        goal.steps = steps;
    }
}

fn verifier_system_prompt(cwd: &str) -> String {
    format!(
        "You are the goal verifier for a coding agent working in {cwd}. \
Judge whether the goal below is met using ONLY concrete evidence from the \
work so far: files changed, commands run, their outputs, test results. \
Plans, intentions, and todo lists are not progress. \
The FIRST line of your reply is exactly one of:\n\
COMPLETE: <the decisive evidence in a few words> — the goal is verifiably met.\n\
CONTINUE: <the single most important missing piece> — work remains.\n\
BLOCKED: <the reason> — the goal cannot be met (missing access, \
contradictory requirements, or outside the agent's ability).\n\
Keep whatever follows the colon to ONE short line — it lands in the chat \
as a status row, never a paragraph.\n\
On a CONTINUE or BLOCKED verdict, two blocks follow the first line:\n\
NAME: <a short name for the goal, two to six words>\n\
STEPS:\n\
- [x] <a step the evidence shows finished>\n\
- [ ] <a step that remains>\n\
Three to seven steps, short imperative phrases. The list mirrors the \
CURRENT state: carry the shown steps forward, flipping one to [x] only on \
concrete evidence."
    )
}

/// The card state the verifier maintains: its own previous name and step
/// list ride the prompt, so it carries them forward instead of reinventing
/// the plan on every pass.
fn card_state_prompt(goal: &ChatGoalState) -> String {
    if goal.name.is_none() && goal.steps.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "The goal card currently shows (keep the name stable; bring the steps up to date):\n",
    );
    if let Some(name) = &goal.name {
        block.push_str(&format!("NAME: {name}\n"));
    }
    if !goal.steps.is_empty() {
        block.push_str("STEPS:\n");
        for step in &goal.steps {
            block.push_str(&format!(
                "- [{}] {}\n",
                if step.done { "x" } else { " " },
                step.text
            ));
        }
    }
    block
}

/// The verifier-driven continuation: the goal plus what the last check
/// found missing, closed by the round's marker line — the UI draws the
/// Turn as a loop row from it instead of a user bubble.
pub(crate) fn continuation_prompt(text: &str, round: &GoalRound) -> String {
    let status = match (&round.kind, round.reason.is_empty()) {
        (GoalRoundKind::Retry, _) => {
            "The last verification pass failed before reaching a verdict — keep working toward the goal.".to_string()
        }
        (GoalRoundKind::Resume, true) => "The user resumed the goal.".to_string(),
        _ => format!(
            "The verifier's last check found the goal not yet met: {}",
            round.reason
        ),
    };
    format!(
        "Work toward this goal:\n\n{text}\n\n{status}\n\n\
Continue. Judge your own progress by concrete evidence — files changed, \
commands run, test results — not by plans or intentions.\n\n{}",
        round.marker()
    )
}

/// The evidence the verifier judges: the History tail (the model-facing
/// record — tool results live here, not in the Transcript), formatted
/// newest-last and capped from the front.
fn evidence_tail(chat: &ChatRuntime) -> String {
    let history = chat.history.read().unwrap_or_else(|e| e.into_inner());
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for message in history.iter().rev() {
        let line = match message {
            AgentMessage::User(user) => {
                let text = match &user.content {
                    pi_core::ai::types::UserContent::Text(text) => text.clone(),
                    pi_core::ai::types::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            pi_core::ai::types::BlockContent::Text(text) => {
                                Some(text.text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                format!("User: {}", truncate(&text, 800))
            }
            AgentMessage::Assistant(assistant) => assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    pi_core::ai::types::AssistantContent::Text(text) => {
                        Some(format!("Assistant: {}", truncate(&text.text, 800)))
                    }
                    pi_core::ai::types::AssistantContent::ToolCall(call) => Some(format!(
                        "Tool call: {} {}",
                        call.name,
                        truncate(
                            &serde_json::to_string(&call.arguments).unwrap_or_default(),
                            200,
                        )
                    )),
                    pi_core::ai::types::AssistantContent::Thinking(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            AgentMessage::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pi_core::ai::types::BlockContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "Tool result{} ({}): {}",
                    if result.is_error { " ERROR" } else { "" },
                    result.tool_name,
                    truncate(&text, TOOL_RESULT_BYTES)
                )
            }
            AgentMessage::Custom(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        bytes += line.len();
        if bytes > EVIDENCE_TAIL_BYTES {
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
}

/// The Turn's frozen change set as one bounded paragraph — the workspace
/// evidence no conversation text can fake.
fn change_set_summary(change_set: Option<&TurnChangeSet>) -> String {
    let Some(change_set) = change_set else {
        return "Workspace changes this turn: unknown (no change set captured).".into();
    };
    if change_set.files.is_empty() {
        return "Workspace changes this turn: none.".into();
    }
    let listed = change_set
        .files
        .iter()
        .take(CHANGE_SET_LISTED)
        .map(|file| format!("{} (+{}/-{})", file.path, file.additions, file.deletions))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = change_set.files.len().saturating_sub(CHANGE_SET_LISTED);
    format!(
        "Workspace changes this turn: {}{}{}",
        listed,
        if rest > 0 {
            format!(", and {rest} more")
        } else {
            String::new()
        },
        if change_set.truncated {
            " (truncated)"
        } else {
            ""
        }
    )
}

/// Whether the settled Turn called any tool — the no-progress counter's
/// activity test, over the History slice since the last user message.
fn turn_called_tools(chat: &ChatRuntime) -> bool {
    let history = chat.history.read().unwrap_or_else(|e| e.into_inner());
    history
        .iter()
        .rev()
        .take_while(|message| !matches!(message, AgentMessage::User(_)))
        .any(|message| match message {
            AgentMessage::Assistant(assistant) => assistant
                .content
                .iter()
                .any(|part| matches!(part, pi_core::ai::types::AssistantContent::ToolCall(_))),
            AgentMessage::ToolResult(_) => true,
            _ => false,
        })
}

fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Mutate one chat row under the registry's persistence lock, then publish.
/// A deleted or unknown chat is a no-op — the loop's writes are all
/// best-effort after the fact.
fn mutate_goal_row(
    service: &EngineService,
    chat_id: &str,
    mutate: impl FnOnce(&mut ChatGoalState),
) -> Option<ChatGoalState> {
    let updated = {
        let mut chats = service
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let row = chats.iter_mut().find(|row| row.id == chat_id)?;
        let goal = row.goal.as_mut()?;
        mutate(goal);
        let updated = goal.clone();
        if let Err(error) = crate::store::persist_chats(&service.data_dir, &chats) {
            tracing::warn!(target: "holt::goal", %error, "goal state could not persist");
        }
        updated
    };
    service.runtime.publish_chats();
    Some(updated)
}

/// Clear the goal entirely (verifier `COMPLETE`, admission failure): the row
/// forgets the objective, queued continuations are swept, an in-flight check
/// is cancelled.
pub(crate) fn clear_goal_state(service: &EngineService, chat: &Arc<ChatRuntime>) {
    cancel_check(chat);
    {
        let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = queue.sweep_goal_continuations() {
            tracing::warn!(target: "holt::goal", %error, "goal continuation sweep failed");
        }
    }
    {
        let mut chats = service
            .runtime
            .chats
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == chat.chat_id) else {
            return;
        };
        row.goal = None;
        if let Err(error) = crate::store::persist_chats(&service.data_dir, &chats) {
            tracing::warn!(target: "holt::goal", %error, "goal clear could not persist");
        }
    }
    service.runtime.publish_chats();
}

/// A goal lifecycle row in the Transcript (ADR-0010 housekeeping: for the
/// reader, never the model — the continuation prompt carries what the model
/// needs).
fn notice(service: &EngineService, chat: &Arc<ChatRuntime>, message: String) {
    crate::decode::push_system_part(
        chat,
        &service.engine_info.device_id,
        format!("goal-{}", uuid::Uuid::new_v4()),
        holt_doc::parts::MessagePart::Notice {
            id: "n0".into(),
            message,
        },
    );
}

/// The end-of-loop status segment: rounds, wall time, and the gross tokens
/// the goal burned since it was armed. Turns, verifier passes, and
/// compactions all book to the same chat ledger, so a timestamp filter
/// catches the whole arc — and this runs after settle flushed the Turn's
/// own batch.
fn goal_stats(chat: &ChatRuntime, goal: &ChatGoalState) -> String {
    let mut parts = Vec::new();
    match goal.iteration {
        0 => {}
        1 => parts.push("1 round".into()),
        n => parts.push(format!("{n} rounds")),
    }
    parts.push(format_duration(
        (Utc::now() - goal.started_at).num_seconds().max(0),
    ));
    let since = goal.started_at.timestamp_millis();
    let tokens = crate::usage::load_records(&chat.data_dir, &chat.chat_id)
        .map(|records| {
            records
                .iter()
                .filter(|record| record.timestamp >= since)
                .map(|record| record.gross())
                .sum::<u64>()
        })
        .unwrap_or(0);
    parts.push(format!("{} tokens", compact_tokens(tokens)));
    parts.join(" · ")
}

/// Wall time as `12 min` / `3 h 5 min` — a status row never needs finer.
fn format_duration(secs: i64) -> String {
    let mins = secs / 60;
    if mins < 1 {
        return "under a minute".to_string();
    }
    if mins < 60 {
        return format!("{mins} min");
    }
    let (hours, rest) = (mins / 60, mins % 60);
    if rest == 0 {
        format!("{hours} h")
    } else {
        format!("{hours} h {rest} min")
    }
}

/// The UI tile's compact count, mirrored engine-side (`45.2k`, `1.1M`).
fn compact_tokens(tokens: u64) -> String {
    const SUFFIXES: [&str; 3] = ["k", "M", "B"];
    if tokens < 1_000 {
        return tokens.to_string();
    }
    // Step the suffix up where the mantissa's own rounding would carry
    // (`999_950` prints as `1000.0k`, so it reads `1M` instead).
    let carries = |suffix: u32| 1000u64.pow(suffix + 1) - 5 * 10u64.pow(3 * suffix - 2);
    let mut suffix = 1u32;
    while (suffix as usize) < SUFFIXES.len() && tokens >= carries(suffix) {
        suffix += 1;
    }
    let value = format!("{:.1}", tokens as f64 / 1000f64.powi(suffix as i32));
    let value = value.trim_end_matches('0').trim_end_matches('.');
    format!("{value}{}", SUFFIXES[suffix as usize - 1])
}

/// Cancel the in-flight verifier pass, if any. The pass resolves to "no
/// verdict" — every cancel source has already changed the goal state it
/// would have written.
pub(crate) fn cancel_check(chat: &ChatRuntime) {
    if let Some(token) = chat
        .goal_check_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        token.cancel();
    }
}

/// The verifier's transport: the configured verifier model first (the
/// ADR-0044 follow-up — a separate judge, ideally on another provider so
/// the loop's blind spots are not the judge's too), else the chat's own
/// model resolved fresh from the row's captured config. A configured model
/// that no longer resolves — or whose provider lost its key — falls back
/// to the chat's own: the loop never stalls on settings drift.
async fn check_transport(
    service: &EngineService,
    chat_id: &str,
) -> Result<(pi_core::ai::types::Model, String, String), String> {
    let (provider, model_id, cwd) = {
        let chats = service
            .runtime
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let row = chats
            .iter()
            .find(|row| row.id == chat_id)
            .ok_or_else(|| "unknown chat".to_string())?;
        let config = row
            .config
            .as_ref()
            .ok_or_else(|| "the chat has no captured model settings".to_string())?;
        let cwd = row
            .cwd
            .clone()
            .ok_or_else(|| "the chat has no working directory".to_string())?;
        (config.provider.clone(), config.model.clone(), cwd)
    };
    if let Some(verifier_id) = service.goal_settings.get().model_id.as_deref()
        && let Some((verifier_provider, _)) = verifier_id.split_once('/')
        && let Ok(model) = service
            .providers
            .resolve_model(verifier_provider, verifier_id)
        && let Some(api_key) = service
            .providers
            .credentials
            .reveal_key(verifier_provider)
            .await
    {
        return Ok((model, api_key, cwd));
    }
    let model = service
        .providers
        .resolve_model(provider.as_str(), &model_id)
        .map_err(|error| error.to_string())?;
    let api_key = service
        .providers
        .credentials
        .reveal_key(provider.as_str())
        .await
        .ok_or_else(|| format!("provider {provider} is not configured"))?;
    Ok((model, api_key, cwd))
}

/// One verification pass: a single completion, no tools, bounded tokens,
/// silent provider retries (the auto-review pass's shape). The reply is
/// booked to the chat's ledger immediately — the Turn's batch settled
/// before this ran, so the capture buffer would strand the record.
async fn run_check(
    service: &EngineService,
    chat: &Arc<ChatRuntime>,
    goal: &ChatGoalState,
    change_set: Option<&TurnChangeSet>,
    cancel: &CancellationToken,
) -> Result<GoalCheckReport, String> {
    let (model, api_key, cwd) = check_transport(service, &chat.chat_id).await?;
    let evidence = evidence_tail(chat);
    let card_state = card_state_prompt(goal);
    let prompt = format!(
        "Goal:\n{}\n\n{}\n\n{}Work so far (newest last):\n{}",
        goal.text,
        change_set_summary(change_set),
        if card_state.is_empty() {
            String::new()
        } else {
            format!("{card_state}\n")
        },
        if evidence.is_empty() {
            "(nothing yet)".to_string()
        } else {
            evidence
        }
    );
    let mut options = pi_core::ai::types::SimpleStreamOptions::default();
    // The judge doesn't solve, it checks: pin the lowest reasoning level
    // (pi maps Minimal to each provider's floor) and leave headroom for the
    // verdict plus the card's name/steps block beside whatever minimal
    // thinking the level emits — a tight cap let reasoning eat the whole
    // budget and garble the reply.
    options.base.max_tokens = Some(3072);
    options.reasoning = Some(pi_core::ai::types::ThinkingLevel::Minimal);
    options.base.base.api_key = Some(api_key);
    options.base.base.signal = Some(cancel.clone());
    options.base.base.max_retries = Some(crate::agent::PROVIDER_MAX_RETRIES);
    let context = pi_core::ai::types::Context {
        system_prompt: Some(verifier_system_prompt(&cwd)),
        messages: vec![pi_core::ai::types::Message::User(
            pi_core::ai::types::UserMessage {
                role: pi_core::ai::types::RoleUser,
                content: pi_core::ai::types::UserContent::Text(prompt),
                timestamp: 0,
            },
        )],
        tools: None,
    };
    let stream_fn = service
        .runtime
        .stream_fn
        .clone()
        .unwrap_or_else(crate::stream::default_stream_fn);
    let stream = stream_fn(&model, &context, Some(&options))
        .map_err(|error| format!("the verifier could not start: {error}"))?;
    let consume = async {
        while let Some(event) = stream.next().await {
            if event.is_terminal() {
                break;
            }
        }
    };
    tokio::select! {
        _ = consume => {}
        _ = cancel.cancelled() => return Err("cancelled".into()),
    }
    let response = stream.result().await;
    crate::usage::record_goal_check(chat, &response);
    let text = response
        .content
        .iter()
        .filter_map(|part| match part {
            pi_core::ai::types::AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    parse_check_report(&text).ok_or_else(|| format!("garbled verdict: {}", truncate(&text, 200)))
}

/// Enqueue the loop's next step — an ordinary, visible queue row flagged as
/// the goal's continuation (the plan follow-up's path). Best-effort: a chat
/// without captured settings or a working directory drops the step, which
/// the restart reconciliation then reads as a stalled loop.
pub(crate) fn enqueue_continuation(
    service: &EngineService,
    chat: &Arc<ChatRuntime>,
    prompt: String,
) -> Result<(), RpcError> {
    let request = {
        let chats = service
            .runtime
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let row = chats
            .iter()
            .find(|row| row.id == chat.chat_id)
            .ok_or_else(|| RpcError::BadParams("unknown chat".into()))?;
        let config = row
            .config
            .clone()
            .ok_or_else(|| RpcError::Failed("the chat has no captured model settings".into()))?;
        let cwd = row
            .cwd
            .clone()
            .ok_or_else(|| RpcError::Failed("the chat has no working directory".into()))?;
        EngineService::queued_run_request(&config, &prompt, cwd)
    };
    let paused = {
        let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
        if chat.is_removed() {
            return Err(RpcError::Failed("chat was deleted".into()));
        }
        queue.enqueue_goal_continuation(request, uuid::Uuid::new_v4().to_string())?;
        queue.paused()
    };
    if paused {
        // The queue's pause wins (ADR-0021): the row stays for the user to
        // inspect, but the loop stands down until resumed.
        mutate_goal_row(service, &chat.chat_id, |goal| {
            goal.status = GoalStatus::Paused;
        });
        notice(
            service,
            chat,
            "Goal paused — the queue is paused. Continue the queue or resume the goal with /goal resume.".into(),
        );
    } else {
        service.kick_queue(chat.clone());
    }
    Ok(())
}

/// The queue-driver hook after a settled real Turn (ADR-0044). Runs after
/// `clear_final_signal`, inside the driver iteration: the verdict's Notice
/// can never overtake the Turn's own card frame, and the continuation it may
/// enqueue is in place before the next admission.
pub(crate) async fn after_turn_settled(
    service: &EngineService,
    chat: &Arc<ChatRuntime>,
    outcome: SettledTurn,
    change_set: Option<&TurnChangeSet>,
) {
    let goal = {
        let chats = service
            .runtime
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = chats.iter().find(|row| row.id == chat.chat_id) else {
            return;
        };
        row.goal.clone()
    };
    let Some(goal) = goal else {
        return;
    };
    if chat.is_removed() {
        return;
    }
    match outcome {
        SettledTurn::Interrupted => {
            if goal.status == GoalStatus::Active {
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
                notice(
                    service,
                    chat,
                    "Goal paused — the turn was interrupted. Resume with /goal resume.".into(),
                );
            }
            return;
        }
        SettledTurn::Failed => {
            if goal.status == GoalStatus::Active {
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
                notice(
                    service,
                    chat,
                    "Goal paused — the last turn failed. Resume with /goal resume.".into(),
                );
            }
            return;
        }
        SettledTurn::Succeeded => {}
    }
    if goal.status != GoalStatus::Active {
        return;
    }
    // A Turn that ends on an unanswered question card waits on the user;
    // the settle after the answer is the one to judge.
    if crate::tools::ask_user::has_pending_question(chat) {
        return;
    }
    // Any pending row — user-typed or an orphaned continuation — means the
    // next step is already spoken for.
    if !chat
        .queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot()
        .pending
        .is_empty()
    {
        return;
    }
    let token = CancellationToken::new();
    {
        let mut slot = chat
            .goal_check_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = slot.replace(token.clone()) {
            previous.cancel();
        }
    }
    let verdict = run_check(service, chat, &goal, change_set, &token).await;
    chat.goal_check_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if token.is_cancelled() {
        // Whoever cancelled already changed the goal state this verdict
        // would have written.
        return;
    }
    let report = match verdict {
        Ok(report) => report,
        Err(error) => {
            let Some(state) = mutate_goal_row(service, &chat.chat_id, |goal| {
                goal.eval_failures += 1;
                goal.last_reason = Some(error.clone());
            }) else {
                return;
            };
            if state.eval_failures >= MAX_EVAL_FAILURES {
                let stats = goal_stats(chat, &goal);
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
                notice(
                    service,
                    chat,
                    format!(
                        "Goal paused · {stats} — the verifier failed {MAX_EVAL_FAILURES} times in a row. Resume with /goal resume."
                    ),
                );
                return;
            }
            // A failed check must not silently stall the loop with the goal
            // still active: queue the next step so the following settle
            // re-verifies. Bounded by the failure counter above; the user
            // queueing work mid-check takes the wheel as usual.
            if !chat
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snapshot()
                .pending
                .is_empty()
            {
                return;
            }
            if enqueue_continuation(
                service,
                chat,
                continuation_prompt(
                    &goal.text,
                    &GoalRound {
                        kind: GoalRoundKind::Retry,
                        reason: "the verifier couldn't reach a verdict".into(),
                    },
                ),
            )
            .is_err()
            {
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
            }
            return;
        }
    };
    let GoalCheckReport {
        verdict,
        name,
        steps,
    } = report;
    match verdict {
        GoalVerdict::Complete(_) => {
            // The end row is a one-line status — the Turn's own final
            // message is the summary, so the verifier's evidence would
            // only duplicate it as a paragraph. It stays in the log.
            let stats = goal_stats(chat, &goal);
            clear_goal_state(service, chat);
            notice(service, chat, format!("Goal achieved · {stats}"));
        }
        GoalVerdict::Blocked(reason) => {
            let stats = goal_stats(chat, &goal);
            mutate_goal_row(service, &chat.chat_id, |goal| {
                goal.status = GoalStatus::Blocked;
                goal.eval_failures = 0;
                goal.last_reason = Some(reason.clone());
                apply_card_state(goal, name, steps);
            });
            notice(service, chat, format!("Goal blocked · {stats} — {reason}"));
        }
        GoalVerdict::Continue(reason) => {
            let called_tools = turn_called_tools(chat);
            let no_changes = change_set.is_none_or(|set| set.files.is_empty());
            let stalled = mutate_goal_row(service, &chat.chat_id, |goal| {
                goal.eval_failures = 0;
                goal.last_reason = Some(reason.clone());
                apply_card_state(goal, name, steps);
                if !called_tools && no_changes {
                    goal.no_progress += 1;
                } else {
                    goal.no_progress = 0;
                }
            });
            let Some(state) = stalled else { return };
            if state.no_progress >= MAX_NO_PROGRESS {
                let stats = goal_stats(chat, &goal);
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
                notice(
                    service,
                    chat,
                    format!(
                        "Goal paused · {stats} — no progress in {MAX_NO_PROGRESS} turns (no tool calls, no workspace changes). Resume with /goal resume."
                    ),
                );
                return;
            }
            if state.iteration >= MAX_ITERATIONS {
                let stats = goal_stats(chat, &goal);
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.status = GoalStatus::Paused;
                });
                notice(
                    service,
                    chat,
                    format!(
                        "Goal paused · {stats} — reached the {MAX_ITERATIONS}-iteration cap. Resume with /goal resume."
                    ),
                );
                return;
            }
            // Re-check before enqueueing: the user may have queued work
            // while the verifier ran — that is the user taking the wheel.
            if !chat
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snapshot()
                .pending
                .is_empty()
            {
                return;
            }
            let round = GoalRound {
                kind: GoalRoundKind::Continue(state.iteration + 1),
                reason,
            };
            if enqueue_continuation(service, chat, continuation_prompt(&goal.text, &round)).is_ok()
            {
                mutate_goal_row(service, &chat.chat_id, |goal| {
                    goal.iteration += 1;
                });
            }
        }
    }
}

/// The second hook (ADR-0044): a Message item's admission failure — the
/// model or credential is gone — clears the goal, since no continuation can
/// fix it. A manual Compaction's failure on the same path never reaches
/// here (the driver distinguishes the kinds).
pub(crate) async fn after_admission_failure(
    service: &EngineService,
    chat: &Arc<ChatRuntime>,
    error: &str,
) {
    let has_goal = {
        let chats = service
            .runtime
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner());
        chats
            .iter()
            .find(|row| row.id == chat.chat_id)
            .is_some_and(|row| row.goal.is_some())
    };
    if !has_goal {
        return;
    }
    clear_goal_state(service, chat);
    notice(
        service,
        chat,
        format!("Goal cleared — the turn could not start: {error}"),
    );
}

/// Deleting the loop's queued continuation is a stop request (ADR-0044):
/// the goal pauses, keeping the objective for an explicit resume.
pub(crate) fn pause_for_deleted_continuation(service: &EngineService, chat: &Arc<ChatRuntime>) {
    let active = {
        let chats = service
            .runtime
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner());
        chats
            .iter()
            .find(|row| row.id == chat.chat_id)
            .and_then(|row| row.goal.as_ref())
            .is_some_and(|goal| goal.status == GoalStatus::Active)
    };
    if !active {
        return;
    }
    mutate_goal_row(service, &chat.chat_id, |goal| {
        goal.status = GoalStatus::Paused;
    });
    notice(
        service,
        chat,
        "Goal paused — its queued continuation was deleted. Resume with /goal resume.".into(),
    );
}

/// Boot reconciliation (ADR-0044): an `active` goal whose queue holds no
/// continuation can never run again — continuations are born only at a Turn
/// settle — so it opens paused. Runs over the registry before any runtime
/// exists; queue files are read directly.
pub(crate) fn reconcile_on_boot(data_dir: &std::path::Path, chats: &mut [Chat]) -> bool {
    let mut changed = false;
    for row in chats.iter_mut() {
        let Some(goal) = &mut row.goal else { continue };
        if goal.status != GoalStatus::Active {
            continue;
        }
        let queue = crate::queue::Queue::load(data_dir, &row.id);
        if queue.has_goal_continuation() {
            continue;
        }
        goal.status = GoalStatus::Paused;
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_stats_format_compactly() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(45_200), "45.2k");
        assert_eq!(compact_tokens(272_000), "272k");
        assert_eq!(compact_tokens(999_950), "1M");
        assert_eq!(format_duration(30), "under a minute");
        assert_eq!(format_duration(720), "12 min");
        assert_eq!(format_duration(3 * 3600), "3 h");
        assert_eq!(format_duration(3 * 3600 + 5 * 60), "3 h 5 min");
    }

    #[test]
    fn verdict_parse_is_strict_first_line() {
        assert_eq!(
            parse_verdict("COMPLETE: the tests pass"),
            Some(GoalVerdict::Complete("the tests pass".into()))
        );
        assert_eq!(
            parse_verdict("CONTINUE: login page still 500s"),
            Some(GoalVerdict::Continue("login page still 500s".into()))
        );
        assert_eq!(
            parse_verdict("BLOCKED: no database access"),
            Some(GoalVerdict::Blocked("no database access".into()))
        );
        // A reason-less CONTINUE/BLOCKED still parses.
        assert_eq!(
            parse_verdict("CONTINUE"),
            Some(GoalVerdict::Continue("no reason given".into()))
        );
        // A bare COMPLETE cites no evidence: no verdict at all, so the
        // caller counts an evaluation failure instead of clearing the goal.
        assert_eq!(parse_verdict("COMPLETE"), None);
        assert_eq!(parse_verdict("COMPLETE:"), None);
        assert_eq!(parse_verdict("COMPLETE:   "), None);
        // Anything else is no verdict — the caller counts a failure,
        // never a fabricated Continue.
        assert_eq!(parse_verdict("I think we should continue"), None);
        assert_eq!(parse_verdict(""), None);
        assert_eq!(parse_verdict("CONTINUED: nope"), None);
    }

    #[test]
    fn check_report_reads_the_card_blocks_after_the_verdict() {
        let report = parse_check_report(
            "CONTINUE: the tests are still missing\n\
             NAME: Ship the login page\n\
             STEPS:\n\
             - [x] build the form\n\
             - [ ] add the tests",
        )
        .expect("a verdict plus card blocks parses");
        assert_eq!(
            report.verdict,
            GoalVerdict::Continue("the tests are still missing".into())
        );
        assert_eq!(report.name.as_deref(), Some("Ship the login page"));
        assert_eq!(
            report.steps,
            vec![
                GoalStep {
                    text: "build the form".into(),
                    done: true,
                },
                GoalStep {
                    text: "add the tests".into(),
                    done: false,
                },
            ]
        );

        // The extras are optional: a bare verdict line leaves the card as
        // it was (the caller keeps the previous name and steps).
        let bare = parse_check_report("BLOCKED: no database access").unwrap();
        assert_eq!(bare.name, None);
        assert!(bare.steps.is_empty());

        // A garbled first line is no verdict at all, even with card blocks.
        assert!(parse_check_report("NAME: huh\n- [x] something").is_none());
    }

    #[test]
    fn check_report_caps_and_skips_sloppy_lines() {
        // Uppercase marks and `*` bullets count; empty step text and
        // non-task lines are skipped; the list caps out.
        let mut reply = String::from(
            "CONTINUE: keep going\nNAME: X\nSTEPS:\n- [X] done one\n* [ ] todo two\n- [ ] \n- not a task\n",
        );
        for ix in 0..10 {
            reply.push_str(&format!("- [ ] step {ix}\n"));
        }
        let report = parse_check_report(&reply).unwrap();
        assert_eq!(report.name.as_deref(), Some("X"));
        assert_eq!(report.steps.len(), MAX_GOAL_STEPS);
        assert!(report.steps[0].done);
        assert_eq!(report.steps[0].text, "done one");
        assert!(!report.steps[1].done);
        assert_eq!(report.steps[1].text, "todo two");

        // A long name trims to the cap; a long step text trims too.
        let long = parse_check_report(&format!(
            "CONTINUE: go\nNAME: {}\nSTEPS:\n- [ ] {}",
            "word ".repeat(30),
            "a".repeat(200)
        ))
        .unwrap();
        assert_eq!(long.name.unwrap().chars().count(), MAX_GOAL_NAME_CHARS);
        assert_eq!(long.steps[0].text.chars().count(), MAX_STEP_CHARS);
    }

    #[test]
    fn card_state_applies_only_when_fresh() {
        let mut goal = ChatGoalState {
            text: "ship it".into(),
            name: None,
            steps: Vec::new(),
            status: GoalStatus::Active,
            iteration: 0,
            no_progress: 0,
            eval_failures: 0,
            started_at: Utc::now(),
            last_reason: None,
        };
        apply_card_state(
            &mut goal,
            Some("Ship it".into()),
            vec![GoalStep {
                text: "build".into(),
                done: false,
            }],
        );
        assert_eq!(goal.name.as_deref(), Some("Ship it"));
        assert_eq!(goal.steps.len(), 1);
        // A reply without the blocks keeps what the card already shows.
        apply_card_state(&mut goal, None, Vec::new());
        assert_eq!(goal.name.as_deref(), Some("Ship it"));
        assert_eq!(goal.steps.len(), 1);
    }

    #[test]
    fn card_state_prompt_echoes_the_shown_plan() {
        let mut goal = ChatGoalState {
            text: "ship it".into(),
            name: None,
            steps: Vec::new(),
            status: GoalStatus::Active,
            iteration: 0,
            no_progress: 0,
            eval_failures: 0,
            started_at: Utc::now(),
            last_reason: None,
        };
        assert!(card_state_prompt(&goal).is_empty());
        goal.name = Some("Ship it".into());
        goal.steps = vec![
            GoalStep {
                text: "build".into(),
                done: true,
            },
            GoalStep {
                text: "test".into(),
                done: false,
            },
        ];
        let prompt = card_state_prompt(&goal);
        assert!(prompt.contains("NAME: Ship it"));
        assert!(prompt.contains("- [x] build"));
        assert!(prompt.contains("- [ ] test"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let text = "a".repeat(100);
        assert_eq!(truncate(&text, 800).len(), 100);
        // A multi-byte char is never split: two crabs (8 bytes) plus the
        // ellipsis, not 10 bytes of broken UTF-8.
        let emoji = "🦀".repeat(100);
        assert_eq!(truncate(&emoji, 10), "🦀🦀…");
    }
}
