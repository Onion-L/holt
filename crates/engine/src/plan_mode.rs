//! Plan Mode (ADR-0025): the chat-level planning checkpoint orthogonal to
//! the permission mode. The state rides the chat row (`Chat::plan_mode`,
//! restored by restart); this module owns the planning-turn shaping the run
//! loop applies to a Turn admitted under Plan Mode: the read-only tool
//! whitelist and the system-prompt block that makes a `<proposed_plan>`
//! Markdown block the only submission channel. The plan itself is ordinary
//! assistant text — no plan documents, no plan tools — and the transcript
//! folds each complete block into an approval card the user resolves.

use holt_doc::MessagePart;
use holt_doc::parts::{PlanApprovalState, PlanApprovalVerdict};

use crate::agent::ChatRuntime;

/// The plan-proposal convention (ADR-0025): the model wraps its complete
/// plan in this block; the transcript folds each block into an approval
/// card.
pub(crate) const PROPOSED_PLAN_OPEN: &str = "<proposed_plan>";
pub(crate) const PROPOSED_PLAN_CLOSE: &str = "</proposed_plan>";

/// The shared read-only tool predicate (ADR-0025): the exploration surface
/// an Explorer subagent gets (ADR-0023) and a planning Turn keeps — nothing
/// that can change files or execute commands.
pub(crate) fn read_only_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "read" | "grep" | "read_chat" | "web_fetch" | "web_search"
    )
}

/// The planning system-prompt block: read-only exploration plus the
/// `<proposed_plan>` convention — ordinary text never reaches approval
/// (ADR-0025), and each block fully replaces the previous one.
pub(crate) fn planning_system_block() -> String {
    format!(
        "## Plan Mode (active)\n\n\
This turn is a PLANNING turn. Explore the workspace and produce an \
implementation plan for the user to review and approve. You must not \
change the workspace.\n\n\
- Your tools are read-only exploration (`read`, `grep`, `read_chat`, \
`web_fetch`, and `web_search` when available). `bash`, `write`, \
`edit`, and subagent delegation are unavailable; do not attempt \
workarounds.\n\
- Settle intent and tradeoffs with the user in ordinary text before \
finalizing; ask rather than guess when an ambiguity is high-impact.\n\
- When the plan is decision complete, present it as ONE complete \
Markdown block wrapped in `{PROPOSED_PLAN_OPEN}` and \
`{PROPOSED_PLAN_CLOSE}`. Only a complete block reaches approval — \
ordinary text, questions, and progress notes never do.\n\
- Each new block fully replaces the previous one; never fragment a \
plan across several blocks in one turn.\n\
- Do not ask \"should I proceed?\". The user will approve the plan or \
reply with feedback; revising on feedback is a normal planning turn.\n"
    )
}

/// One segment of assistant text split around `<proposed_plan>` blocks.
#[derive(Debug, PartialEq)]
pub(crate) enum PlanSegment {
    /// Text outside any block — rendered as ordinary prose.
    Text(String),
    /// The Markdown inside one complete block — folded into an approval
    /// card. Unterminated blocks stay ordinary text (a stream may cut a
    /// message mid-block; only complete blocks propose).
    Plan(String),
}

/// Split assistant text around complete `<proposed_plan>` blocks. The tags
/// themselves are dropped; adjacent non-block text merges into single
/// segments.
pub(crate) fn split_plan_blocks(text: &str) -> Vec<PlanSegment> {
    let mut segments: Vec<PlanSegment> = Vec::new();
    let mut tail = text;
    while let Some(open) = tail.find(PROPOSED_PLAN_OPEN) {
        let content_start = open + PROPOSED_PLAN_OPEN.len();
        let close = tail[content_start..]
            .find(PROPOSED_PLAN_CLOSE)
            .map(|i| content_start + i);
        let Some(close) = close else {
            // Unterminated block: ordinary text for now.
            break;
        };
        push_text(&mut segments, &tail[..open]);
        segments.push(PlanSegment::Plan(
            tail[content_start..close].trim().to_string(),
        ));
        tail = &tail[close + PROPOSED_PLAN_CLOSE.len()..];
    }
    push_text(&mut segments, tail);
    segments
}

fn push_text(segments: &mut Vec<PlanSegment>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(PlanSegment::Text(existing)) = segments.last_mut() {
        existing.push_str(text);
    } else {
        segments.push(PlanSegment::Text(text.to_string()));
    }
}

/// Fold assistant text into transcript parts: ordinary text stays prose,
/// each complete `<proposed_plan>` block becomes a pending approval card.
/// `next_id` mints unique part ids within the entry.
pub(crate) fn plan_aware_text_parts(
    text: &str,
    next_id: &mut dyn FnMut() -> String,
) -> Vec<MessagePart> {
    let mut parts = Vec::new();
    for segment in split_plan_blocks(text) {
        match segment {
            PlanSegment::Text(text) if !text.trim().is_empty() => parts.push(MessagePart::Text {
                id: next_id(),
                text,
            }),
            PlanSegment::Text(_) => {}
            PlanSegment::Plan(content) if !content.is_empty() => {
                parts.push(MessagePart::PlanApproval {
                    id: next_id(),
                    content,
                    state: PlanApprovalState::Pending,
                });
            }
            PlanSegment::Plan(_) => {}
        }
    }
    parts
}

/// Whether the chat's transcript carries at least one pending approval
/// card — the guard `ResolvePlanApproval` refuses on.
pub(crate) fn has_pending_plan_cards(chat_id: &str, runtime: &crate::agent::AgentRuntime) -> bool {
    let Some(chat) = runtime.loaded_chat(chat_id) else {
        return false;
    };
    chat.transcript
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|entry| {
            entry.parts.iter().any(|part| {
                matches!(
                    part,
                    MessagePart::PlanApproval {
                        state: PlanApprovalState::Pending,
                        ..
                    }
                )
            })
        })
}

/// Settle every still-pending approval card in the transcript to `verdict`
/// (ADR-0025). A resolution is pure display state — the lifecycle moved
/// through the RPC — so this is a transcript edit only. Blocks proposed
/// across turns all address the chat's Plan Mode; a verdict settles them
/// together.
pub(crate) fn settle_plan_cards(chat: &ChatRuntime, verdict: PlanApprovalVerdict) {
    let mut transcript = chat
        .transcript
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let mut changed = false;
    for entry in transcript.iter_mut() {
        for part in entry.parts.iter_mut() {
            if let MessagePart::PlanApproval { state, .. } = part
                && *state == PlanApprovalState::Pending
            {
                *state = PlanApprovalState::Settled { verdict };
                changed = true;
            }
        }
    }
    drop(transcript);
    if changed {
        chat.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_planning_whitelist_keeps_reads_and_drops_everything_mutating() {
        for name in ["read", "grep", "read_chat", "web_fetch", "web_search"] {
            assert!(read_only_tool_allowed(name), "{name} should survive");
        }
        for name in ["write", "edit", "bash", "Agent", "read_chat_x", "Bash"] {
            assert!(!read_only_tool_allowed(name), "{name} must be dropped");
        }
    }

    #[test]
    fn the_planning_block_makes_the_block_the_only_submission_channel() {
        let block = planning_system_block();
        assert!(block.contains("## Plan Mode (active)"));
        assert!(block.contains(PROPOSED_PLAN_OPEN));
        assert!(block.contains("Only a complete block reaches approval"));
        assert!(block.contains("read-only exploration"));
    }

    #[test]
    fn splits_blocks_from_ordinary_text() {
        let text = "intro\n\n<proposed_plan>\n# Plan\n- step\n</proposed_plan>\n\noutro";
        assert_eq!(
            split_plan_blocks(text),
            vec![
                PlanSegment::Text("intro\n\n".into()),
                PlanSegment::Plan("# Plan\n- step".into()),
                PlanSegment::Text("\n\noutro".into()),
            ]
        );
    }

    #[test]
    fn unterminated_blocks_stay_ordinary_text() {
        let text = "<proposed_plan>\npartial";
        assert_eq!(
            split_plan_blocks(text),
            vec![PlanSegment::Text(text.into())]
        );
    }

    #[test]
    fn every_complete_block_becomes_a_card_even_in_a_series() {
        let text = "<proposed_plan>a</proposed_plan>mid<proposed_plan>b</proposed_plan>";
        assert_eq!(
            split_plan_blocks(text),
            vec![
                PlanSegment::Plan("a".into()),
                PlanSegment::Text("mid".into()),
                PlanSegment::Plan("b".into()),
            ]
        );
    }

    #[test]
    fn plan_aware_parts_strip_tags_and_name_cards() {
        let mut counter = 0usize;
        let parts = plan_aware_text_parts(
            "thinking\n<proposed_plan>\n# Plan\n</proposed_plan>",
            &mut || {
                counter += 1;
                format!("p{counter}")
            },
        );
        assert_eq!(
            parts,
            vec![
                MessagePart::Text {
                    id: "p1".into(),
                    text: "thinking\n".into(),
                },
                MessagePart::PlanApproval {
                    id: "p2".into(),
                    content: "# Plan".into(),
                    state: PlanApprovalState::Pending,
                },
            ]
        );
    }

    #[test]
    fn pending_cards_settle_together_across_entries() {
        use holt_doc::MessageRole;
        use holt_proto::WorkspaceScope;

        let dir = tempfile::tempdir().unwrap();
        let runtime = crate::agent::AgentRuntime::new(
            "dev".into(),
            WorkspaceScope::Local,
            dir.path().to_path_buf(),
            Vec::new(),
            None,
        );
        let chat = runtime.chat("chat-1");
        let part = |id: &str, state: PlanApprovalState| MessagePart::PlanApproval {
            id: id.into(),
            content: "# plan".into(),
            state,
        };
        for id in ["p1", "p2"] {
            chat.transcript
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push(holt_doc::SessionMessageEntry {
                    id: id.into(),
                    role: MessageRole::System,
                    parts: vec![part(id, PlanApprovalState::Pending)],
                    created_at: 0,
                    device_id: "dev".into(),
                    status: None,
                    continuation_of: None,
                });
        }
        settle_plan_cards(&chat, PlanApprovalVerdict::Approved);
        for entry in chat
            .transcript
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let MessagePart::PlanApproval { state, .. } = &entry.parts[0] else {
                panic!("expected a card");
            };
            assert!(matches!(
                state,
                PlanApprovalState::Settled {
                    verdict: PlanApprovalVerdict::Approved
                }
            ));
        }
        // The chat row keeps its planning state untouched: the card settle
        // is display-only (the row was never involved).
        assert!(
            runtime
                .chats
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }
}
