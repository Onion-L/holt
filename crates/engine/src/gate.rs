//! The permission gate (ADR-0014): the confirm-changes gatekeeper wired
//! through the agent loop's before-tool-call hook. Every mutating tool
//! call (write, edit, bash) pauses the Turn behind a pending Approval the
//! UI resolves over the `ResolveApproval` RPC; denials settle as error
//! tool results the model reads, and interrupt cancels the wait. Reads
//! and content search are never gated, and full-access never reaches this
//! module. Auto-review (its tier) slots in beside the approval path later.

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

/// Build the gate's before-tool-call hook for one run. `mode` is the
/// Turn's snapshot (ADR-0014): switches mid-Turn leave the running Turn
/// under its original mode. The hook blocks in the verdict wait and races
/// the run's cancellation token, so no approval can outlive its Turn.
pub(crate) fn before_tool_call_hook(
    mode: PermissionMode,
    chat: Arc<ChatRuntime>,
    base_parts: Arc<Mutex<Vec<holt_doc::MessagePart>>>,
    approvals: Arc<ApprovalRegistry>,
    cancel: CancellationToken,
) -> BeforeToolCallFn {
    Arc::new(
        move |ctx: BeforeToolCallContext, signal: Option<CancellationToken>| {
            let chat = chat.clone();
            let base_parts = base_parts.clone();
            let approvals = approvals.clone();
            // The loop's own signal — a clone of the run token today, but
            // the hook must not assume that; fall back to the captured one.
            let cancel = signal.unwrap_or_else(|| cancel.clone());
            Box::pin(async move {
                // Auto-review slots in here when its slice lands; until then
                // only confirm-changes gates.
                if mode != PermissionMode::ConfirmChanges || !is_mutating_tool(&ctx.tool_call.name)
                {
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
                            id: approval_id,
                            state: ToolGateState::Settled {
                                verdict: GateVerdict::Aborted,
                            },
                        },
                    );
                    return None;
                };
                match verdict {
                    ApprovalVerdict::Allow => {
                        stamp_gate(
                            &chat,
                            &base_parts,
                            &ctx.tool_call.id,
                            ToolGate {
                                id: approval_id,
                                state: ToolGateState::Settled {
                                    verdict: GateVerdict::Allowed,
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
                id: format!("approval-{id}"),
                state,
            }),
        }
    }

    #[test]
    fn the_gate_predicates_on_tool_identity_only() {
        assert!(is_mutating_tool("write"));
        assert!(is_mutating_tool("edit"));
        assert!(is_mutating_tool("bash"));
        assert!(!is_mutating_tool("read"));
        assert!(!is_mutating_tool("grep"));
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
