//! The permission gate (ADR-0014): the confirm-changes gatekeeper wired
//! through the agent loop's before-tool-call hook. Every mutating tool
//! call (write, edit, bash) pauses the Turn behind a pending Approval the
//! UI resolves over the `ResolveApproval` RPC; denials settle as error
//! tool results the model reads, and interrupt cancels the wait. Reads
//! and content search are never gated, and full-access never reaches this
//! module. Auto-review rides the same hook: one model pass per mutating
//! call, settled straight to its verdict.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use holt_doc::parts::{GateVerdict, ToolGate, ToolGateState};
use holt_proto::{ApprovalVerdict, PermissionMode};
use pi_core::agent::types::{BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult};
use tokio_util::sync::CancellationToken;

use crate::agent::ChatRuntime;

/// The gate predicate (ADR-0014): tool identity only — no command parsing,
/// no path confinement, no read-only tier. Every mutating call meets the
/// chat's gatekeeper regardless of its target.
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "bash")
}

/// The denial reason when the user denies without a written note. A noted
/// denial uses the note verbatim — it is addressed to the model.
pub(crate) const STANDARD_DENIAL: &str = "The user denied this operation.";

/// One chat's always-allow grants (ADR-0014): in-memory, session-scoped,
/// never persisted — cleared on restart. Bash grants match by command
/// prefix, write/edit grants by exact resolved file path. Checked BEFORE
/// the gatekeeper, so they hold across mode switches.
#[derive(Default)]
pub(crate) struct GateGrants {
    bash_prefixes: Vec<String>,
    file_paths: Vec<String>,
}

impl GateGrants {
    /// Record what an always-allow verdict granted: the command's text for
    /// bash, the resolved absolute path for write/edit.
    pub(crate) fn record(&mut self, tool: &str, arguments: &serde_json::Value, cwd: &str) {
        match tool {
            "bash" => {
                if let Some(command) = arg_str(arguments, "command")
                    && !self.bash_prefixes.iter().any(|p| p == &command)
                {
                    self.bash_prefixes.push(command);
                }
            }
            "write" | "edit" => {
                if let Some(path) =
                    arg_str(arguments, "path").map(|p| crate::tools::to_absolute(cwd, &p))
                    && !self.file_paths.contains(&path)
                {
                    self.file_paths.push(path);
                }
            }
            _ => {}
        }
    }

    /// Does a grant pass this call through without asking?
    pub(crate) fn passes(&self, tool: &str, arguments: &serde_json::Value, cwd: &str) -> bool {
        match tool {
            "bash" => arg_str(arguments, "command").is_some_and(|command| {
                self.bash_prefixes
                    .iter()
                    .any(|p| bash_prefix_matches(p, &command))
            }),
            "write" | "edit" => arg_str(arguments, "path")
                .map(|path| crate::tools::to_absolute(cwd, &path))
                .is_some_and(|resolved| {
                    self.file_paths
                        .iter()
                        .any(|allowed| file_path_matches(allowed, &resolved))
                }),
            _ => false,
        }
    }
}

fn arg_str(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

/// Pure: bash grants match by string prefix on the command — no shell
/// parsing, deliberately (ADR-0014): allowing `cargo test` also covers
/// `cargo test -- --nocapture`.
pub(crate) fn bash_prefix_matches(prefix: &str, command: &str) -> bool {
    command.starts_with(prefix)
}

/// Pure: write/edit grants match by exact path.
pub(crate) fn file_path_matches(allowed: &str, path: &str) -> bool {
    allowed == path
}

/// Engine-wide open approvals: approval id → the verdict channel the
/// blocked run is waiting on. One live sender per opening — the entry's
/// removal IS the single-use guarantee. The UI-facing approval payload
/// (tool identity, arguments) lives on the transcript's gate chip, not
/// here.
pub(crate) type ApprovalRegistry =
    Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalVerdict>>>;

/// Stamp a gate onto the run's Tool chip — in the live transcript entry
/// AND in the run's base parts. Both matter: later assistant messages of
/// the same run rebuild the entry from the base, so a stamp that only
/// touched the entry would be wiped by the next tool round (the same
/// rebuild that resets a resolved chip's output between rounds).
pub(crate) fn stamp_gate(
    chat: &ChatRuntime,
    base_parts: &Arc<Mutex<Vec<holt_doc::MessagePart>>>,
    tool_call_id: &str,
    gate: ToolGate,
) {
    let stamped = |part: &mut holt_doc::MessagePart| {
        if let holt_doc::MessagePart::Tool { gate: slot, id, .. } = part
            && id == tool_call_id
        {
            *slot = Some(gate.clone());
            true
        } else {
            false
        }
    };
    let mut base = base_parts.lock().unwrap_or_else(|error| error.into_inner());
    base.iter_mut().any(stamped);
    drop(base);
    let mut transcript = chat
        .transcript
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let changed = transcript
        .iter_mut()
        .any(|entry| entry.parts.iter_mut().any(stamped));
    drop(transcript);
    if changed {
        chat.publish();
    }
}

/// What the run needs to make one review pass (ADR-0014): the chat's own
/// model through the same provider transport the run uses — no
/// separately-configured reviewer.
#[derive(Clone)]
pub(crate) struct ReviewTransport {
    pub(crate) model: pi_core::ai::types::Model,
    pub(crate) api_key: String,
    pub(crate) stream_fn: pi_core::agent::types::StreamFn,
}

/// The system prompt every review pass rides on. The cwd anchors the
/// reviewer; the reply protocol is exact because [`parse_review_reply`]
/// is strict.
fn review_system_prompt(cwd: &str) -> String {
    format!(
        "You are the permission reviewer for a coding agent working in {cwd}. \
Decide whether the tool call below is safe to execute exactly as written. \
Reply with exactly one line and nothing else:\n\
APPROVE — the call is safe to run.\n\
REJECT: <one-line reason> — it is not."
    )
}

/// A review pass's outcome.
pub(crate) enum ReviewOutcome {
    Approve,
    Reject {
        reason: String,
    },
    /// The Turn was cancelled mid-review — no verdict, no stamp.
    Cancelled,
}

/// The standard reason when the reviewer rejects without stating one, and
/// when its reply is not a clear verdict at all (fail closed: a permission
/// gate must never open on a garbled answer).
pub(crate) const REVIEW_UNCLEAR: &str = "The permission reviewer did not give a clear verdict.";

/// Parse the reviewer's reply: `APPROVE` passes; `REJECT: reason` rejects
/// with the stated reason (a bare `REJECT` uses the standard one);
/// anything else rejects as unclear. Pure — unit-tested.
pub(crate) fn parse_review_reply(reply: &str) -> ReviewOutcome {
    let first = reply.trim().lines().next().unwrap_or_default().trim();
    if first.starts_with("APPROVE") {
        return ReviewOutcome::Approve;
    }
    if let Some(reason) = first.strip_prefix("REJECT") {
        let reason = reason.trim().trim_start_matches(':').trim();
        return ReviewOutcome::Reject {
            reason: if reason.is_empty() {
                REVIEW_UNCLEAR.to_string()
            } else {
                reason.to_string()
            },
        };
    }
    ReviewOutcome::Reject {
        reason: REVIEW_UNCLEAR.to_string(),
    }
}

/// One review pass: a single completion through the run's own model and
/// transport, no tools, cancellation-aware (the same race the compaction
/// requests use). Provider failures reject — fail closed, visibly.
async fn run_review_pass(
    review: &ReviewTransport,
    tool_name: &str,
    arguments: &serde_json::Value,
    cwd: &str,
    cancel: &CancellationToken,
) -> ReviewOutcome {
    let prompt = format!(
        "Tool: {tool_name}\nArguments: {arguments}",
        arguments = serde_json::to_string(arguments).unwrap_or_default()
    );
    let mut options = pi_core::ai::types::SimpleStreamOptions::default();
    // The protocol is one line; a small cap bounds a rambling reviewer (the
    // parser reads the first line regardless).
    options.base.max_tokens = Some(256);
    options.base.base.api_key = Some(review.api_key.clone());
    options.base.base.signal = Some(cancel.clone());
    let context = pi_core::ai::types::Context {
        system_prompt: Some(review_system_prompt(cwd)),
        messages: vec![pi_core::ai::types::Message::User(
            pi_core::ai::types::UserMessage {
                role: pi_core::ai::types::RoleUser,
                content: pi_core::ai::types::UserContent::Text(prompt),
                timestamp: 0,
            },
        )],
        tools: None,
    };
    let stream = match (review.stream_fn)(&review.model, &context, Some(&options)) {
        Ok(stream) => stream,
        Err(error) => {
            return ReviewOutcome::Reject {
                reason: format!("the review pass failed: {error}"),
            };
        }
    };
    let consume = async {
        while let Some(event) = stream.next().await {
            if event.is_terminal() {
                break;
            }
        }
    };
    tokio::select! {
        _ = consume => {}
        _ = cancel.cancelled() => return ReviewOutcome::Cancelled,
    }
    let response = stream.result().await;
    match response.stop_reason {
        // Aborted with the token live is the cancellation path (no verdict,
        // no stamp). An abort the gate did not ask for is a garbled
        // outcome — fail closed like every other unclear one.
        pi_core::ai::types::StopReason::Aborted if cancel.is_cancelled() => {
            ReviewOutcome::Cancelled
        }
        pi_core::ai::types::StopReason::Aborted | pi_core::ai::types::StopReason::Error => {
            ReviewOutcome::Reject {
                reason: format!(
                    "the review pass failed: {}",
                    response
                        .error_message
                        .unwrap_or_else(|| "unknown error".into())
                ),
            }
        }
        _ => parse_review_reply(&pi_core::ai::utils::text::content_text(
            &response.content,
            "\n",
        )),
    }
}

/// Build the gate's before-tool-call hook for one run. `mode` is the
/// Turn's snapshot (ADR-0014): switches mid-Turn leave the running Turn
/// under its original mode. The hook blocks in the verdict wait and races
/// the run's cancellation token, so no approval can outlive its Turn.
pub(crate) fn before_tool_call_hook(
    mode: PermissionMode,
    chat: Arc<ChatRuntime>,
    base_parts: Arc<Mutex<Vec<holt_doc::MessagePart>>>,
    approvals: Arc<ApprovalRegistry>,
    cwd: String,
    review: ReviewTransport,
    cancel: CancellationToken,
) -> BeforeToolCallFn {
    Arc::new(
        move |ctx: BeforeToolCallContext, signal: Option<CancellationToken>| {
            let chat = chat.clone();
            let base_parts = base_parts.clone();
            let approvals = approvals.clone();
            let cwd = cwd.clone();
            let review = review.clone();
            // The loop's own signal — a clone of the run token today, but
            // the hook must not assume that; fall back to the captured one.
            let cancel = signal.unwrap_or_else(|| cancel.clone());
            Box::pin(async move {
                if !is_mutating_tool(&ctx.tool_call.name) {
                    return None;
                }
                // Grants are checked BEFORE the gatekeeper (ADR-0014), so
                // they hold across mode switches; only a mode with no
                // gatekeeper (full-access) records no artifacts at all.
                let arguments = ctx
                    .args
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                if mode != PermissionMode::FullAccess
                    && chat
                        .grants
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .passes(&ctx.tool_call.name, &arguments, &cwd)
                {
                    stamp_gate(
                        &chat,
                        &base_parts,
                        &ctx.tool_call.id,
                        ToolGate {
                            origin: None,
                            id: uuid::Uuid::new_v4().to_string(),
                            state: ToolGateState::Settled {
                                verdict: GateVerdict::Exempted,
                            },
                        },
                    );
                    return None;
                }
                if mode == PermissionMode::AutoReview {
                    // No human, no Approval: the chat's own model judges,
                    // and the chip settles straight to its verdict.
                    return match run_review_pass(
                        &review,
                        &ctx.tool_call.name,
                        &arguments,
                        &cwd,
                        &cancel,
                    )
                    .await
                    {
                        ReviewOutcome::Approve => {
                            stamp_gate(
                                &chat,
                                &base_parts,
                                &ctx.tool_call.id,
                                ToolGate {
                                    origin: None,
                                    id: uuid::Uuid::new_v4().to_string(),
                                    state: ToolGateState::Settled {
                                        verdict: GateVerdict::ReviewPassed,
                                    },
                                },
                            );
                            None
                        }
                        ReviewOutcome::Reject { reason } => {
                            stamp_gate(
                                &chat,
                                &base_parts,
                                &ctx.tool_call.id,
                                ToolGate {
                                    origin: None,
                                    id: uuid::Uuid::new_v4().to_string(),
                                    state: ToolGateState::Settled {
                                        verdict: GateVerdict::ReviewRejected {
                                            reason: Some(reason.clone()),
                                        },
                                    },
                                },
                            );
                            Some(BeforeToolCallResult {
                                block: Some(true),
                                reason: Some(reason),
                                terminate: None,
                            })
                        }
                        // Interrupted mid-review: no verdict to record —
                        // the loop's cancellation check settles the call.
                        ReviewOutcome::Cancelled => None,
                    };
                }
                if mode != PermissionMode::ConfirmChanges {
                    return None;
                }
                let approval_id = uuid::Uuid::new_v4().to_string();
                let (resolve, verdict_rx) = tokio::sync::oneshot::channel();
                approvals
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(approval_id.clone(), resolve);
                stamp_gate(
                    &chat,
                    &base_parts,
                    &ctx.tool_call.id,
                    ToolGate {
                        origin: None,
                        id: approval_id.clone(),
                        state: ToolGateState::Pending,
                    },
                );
                // The wait: a verdict through the RPC, or the Turn's end —
                // interrupt remains the only stop-the-run channel.
                let verdict = tokio::select! {
                    verdict = verdict_rx => verdict.ok(),
                    () = cancel.cancelled() => None,
                };
                approvals
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&approval_id);
                let Some(verdict) = verdict else {
                    // Interrupted (or the registry dropped the opening): the
                    // call settles as aborted and the loop's own cancellation
                    // check turns the tool result into an abort.
                    stamp_gate(
                        &chat,
                        &base_parts,
                        &ctx.tool_call.id,
                        ToolGate {
                            origin: None,
                            id: approval_id,
                            state: ToolGateState::Settled {
                                verdict: GateVerdict::Aborted,
                            },
                        },
                    );
                    return None;
                };
                match verdict {
                    ApprovalVerdict::Allow | ApprovalVerdict::AlwaysAllow => {
                        // An always-allow also records the session grant —
                        // in-memory, chat-scoped, gone on restart.
                        if matches!(verdict, ApprovalVerdict::AlwaysAllow) {
                            chat.grants
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .record(&ctx.tool_call.name, &arguments, &cwd);
                        }
                        stamp_gate(
                            &chat,
                            &base_parts,
                            &ctx.tool_call.id,
                            ToolGate {
                                origin: None,
                                id: approval_id,
                                state: ToolGateState::Settled {
                                    verdict: if matches!(verdict, ApprovalVerdict::AlwaysAllow) {
                                        GateVerdict::AlwaysAllowed
                                    } else {
                                        GateVerdict::Allowed
                                    },
                                },
                            },
                        );
                        None
                    }
                    ApprovalVerdict::Deny { note } => {
                        let note = note
                            .map(|note| note.trim().to_string())
                            .filter(|note| !note.is_empty());
                        stamp_gate(
                            &chat,
                            &base_parts,
                            &ctx.tool_call.id,
                            ToolGate {
                                origin: None,
                                id: approval_id,
                                state: ToolGateState::Settled {
                                    verdict: GateVerdict::Denied { note: note.clone() },
                                },
                            },
                        );
                        let reason = note.unwrap_or_else(|| STANDARD_DENIAL.to_string());
                        Some(BeforeToolCallResult {
                            block: Some(true),
                            reason: Some(reason),
                            terminate: None,
                        })
                    }
                }
            })
        },
    )
}

/// Settle any gate still pending on load (ADR-0014): an engine restart
/// ends the Turn that opened it, so the call settles as aborted — the
/// persisted chip replays settled, never eternally pending.
pub(crate) fn settle_pending_gates_on_load(transcript: &mut [holt_doc::SessionMessageEntry]) {
    for entry in transcript {
        for part in &mut entry.parts {
            if let holt_doc::MessagePart::Tool {
                gate: Some(gate), ..
            } = part
                && gate.state == ToolGateState::Pending
            {
                gate.state = ToolGateState::Settled {
                    verdict: GateVerdict::Aborted,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_doc::{MessagePart, SessionMessageEntry};

    fn gated_tool(id: &str, state: ToolGateState) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: holt_proto::ToolCall::Exec {
                command: "ls".into(),
            },
            is_error: false,
            resolved: false,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            gate: Some(ToolGate {
                origin: None,
                id: format!("approval-{id}"),
                state,
            }),
        }
    }

    #[test]
    fn bash_grants_match_by_string_prefix_only() {
        // Allowing `cargo test` covers its longer invocations.
        assert!(bash_prefix_matches("cargo test", "cargo test"));
        assert!(bash_prefix_matches(
            "cargo test",
            "cargo test -- --nocapture"
        ));
        assert!(bash_prefix_matches("cargo test", "cargo tests"));
        // A shorter command or a different command never matches — no
        // shell parsing, plain string prefix. (An empty prefix would match
        // everything, but empty commands are never recorded.)
        assert!(!bash_prefix_matches("cargo test", "cargo"));
        assert!(!bash_prefix_matches("cargo test", "cargo build"));
    }

    #[test]
    fn file_grants_match_by_exact_path_only() {
        assert!(file_path_matches("/repo/src/a.rs", "/repo/src/a.rs"));
        assert!(!file_path_matches("/repo/src/a.rs", "/repo/src/b.rs"));
        assert!(!file_path_matches("/repo/src/a.rs", "/repo/src/a.rs2"));
        assert!(!file_path_matches("/repo/src", "/repo/src/a.rs"));
    }

    #[test]
    fn grants_record_and_pass_calls_resolved_against_the_cwd() {
        let mut grants = GateGrants::default();
        grants.record(
            "bash",
            &serde_json::json!({ "command": "cargo test" }),
            "/repo",
        );
        grants.record(
            "write",
            &serde_json::json!({ "path": "src/a.rs", "content": "x" }),
            "/repo",
        );
        // Bash: the allowed prefix and its extensions pass; others don't.
        assert!(grants.passes(
            "bash",
            &serde_json::json!({ "command": "cargo test -- --nocapture" }),
            "/repo"
        ));
        assert!(!grants.passes(
            "bash",
            &serde_json::json!({ "command": "cargo build" }),
            "/repo"
        ));
        // Write/edit: the exact resolved path — reached relative or
        // absolute — passes; sibling paths don't.
        assert!(grants.passes("write", &serde_json::json!({ "path": "src/a.rs" }), "/repo"));
        assert!(grants.passes(
            "write",
            &serde_json::json!({ "path": "/repo/src/a.rs" }),
            "/repo"
        ));
        assert!(!grants.passes("edit", &serde_json::json!({ "path": "src/b.rs" }), "/repo"));
        // Reads are never granted.
        assert!(!grants.passes("read", &serde_json::json!({ "path": "src/a.rs" }), "/repo"));
        // A different cwd resolves the same relative path elsewhere.
        assert!(!grants.passes(
            "write",
            &serde_json::json!({ "path": "src/a.rs" }),
            "/other"
        ));
        // Recording twice keeps one grant of each kind.
        grants.record(
            "bash",
            &serde_json::json!({ "command": "cargo test" }),
            "/repo",
        );
        grants.record("write", &serde_json::json!({ "path": "src/a.rs" }), "/repo");
        let GateGrants {
            bash_prefixes,
            file_paths,
        } = grants;
        assert_eq!(bash_prefixes, ["cargo test"]);
        assert_eq!(file_paths, ["/repo/src/a.rs"]);
    }

    #[test]
    fn review_replies_parse_strictly_and_fail_closed() {
        // The exact protocol passes.
        assert!(matches!(
            parse_review_reply("APPROVE"),
            ReviewOutcome::Approve
        ));
        assert!(matches!(
            parse_review_reply("  APPROVE  \nand nothing else"),
            ReviewOutcome::Approve
        ));
        // A rejection keeps its stated reason.
        match parse_review_reply("REJECT: use pnpm, not npm") {
            ReviewOutcome::Reject { reason } => assert_eq!(reason, "use pnpm, not npm"),
            _ => panic!("expected a rejection"),
        }
        // A bare rejection and an unclear reply both fall to the standard
        // reason — the gate never opens on a garbled answer.
        for reply in [
            "REJECT",
            "reject: lowercase",
            "I am not sure about this one",
            "",
        ] {
            match parse_review_reply(reply) {
                ReviewOutcome::Reject { reason } => assert_eq!(reason, REVIEW_UNCLEAR),
                _ => panic!("expected a fail-closed rejection for {reply:?}"),
            }
        }
    }

    #[test]
    fn the_gate_predicates_on_tool_identity_only() {
        assert!(is_mutating_tool("write"));
        assert!(is_mutating_tool("edit"));
        assert!(is_mutating_tool("bash"));
        assert!(!is_mutating_tool("read"));
        assert!(!is_mutating_tool("grep"));
        // The web tools are read-tier (ADR-0023): fetch reads a page and
        // search queries one, so neither ever meets the gate.
        assert!(!is_mutating_tool("web_fetch"));
        assert!(!is_mutating_tool("web_search"));
        // Tool identity means identity: lookalikes are not the trio.
        assert!(!is_mutating_tool("Bash"));
        assert!(!is_mutating_tool("bash_safe"));
        assert!(!is_mutating_tool("writefile"));
    }

    #[test]
    fn pending_gates_settle_to_aborted_on_load() {
        let mut transcript = vec![
            SessionMessageEntry {
                id: "entry-1".into(),
                role: holt_doc::MessageRole::Assistant,
                parts: vec![
                    gated_tool("call-1", ToolGateState::Pending),
                    gated_tool(
                        "call-2",
                        ToolGateState::Settled {
                            verdict: GateVerdict::Allowed,
                        },
                    ),
                    MessagePart::Text {
                        id: "t0".into(),
                        text: "hi".into(),
                    },
                ],
                created_at: 0,
                device_id: "device".into(),
                status: None,
                continuation_of: None,
            },
            SessionMessageEntry {
                id: "entry-2".into(),
                role: holt_doc::MessageRole::Assistant,
                parts: vec![gated_tool(
                    "call-3",
                    ToolGateState::Settled {
                        verdict: GateVerdict::Denied {
                            note: Some("no".into()),
                        },
                    },
                )],
                created_at: 1,
                device_id: "device".into(),
                status: None,
                continuation_of: None,
            },
        ];
        settle_pending_gates_on_load(&mut transcript);
        let gate = |entry: usize, part: usize| {
            let MessagePart::Tool { gate, .. } = &transcript[entry].parts[part] else {
                panic!("expected a tool part");
            };
            gate.clone().expect("gate present")
        };
        assert_eq!(
            gate(0, 0).state,
            ToolGateState::Settled {
                verdict: GateVerdict::Aborted
            }
        );
        // Settled verdicts are never rewritten.
        assert_eq!(
            gate(0, 1).state,
            ToolGateState::Settled {
                verdict: GateVerdict::Allowed
            }
        );
        assert_eq!(
            gate(1, 0).state,
            ToolGateState::Settled {
                verdict: GateVerdict::Denied {
                    note: Some("no".into())
                }
            }
        );
    }
}
