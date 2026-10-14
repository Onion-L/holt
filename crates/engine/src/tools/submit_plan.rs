//! The planning Turn's structured submission channel (ADR-plan-mode-is-an-independent-approval-checkpoint): the
//! `submit_plan` tool hands a complete plan to the approval card as typed
//! data, so submission never depends on parsing protocol tags back out of
//! assistant text. The `<proposed_plan>` text convention stays as the
//! fallback — a model that writes the block still folds into a card exactly
//! as before. Like `ask_user`, the call never waits: the card lands from
//! the result details and the model ends its Turn; the user resolves it on
//! the composer's approval bar through the same `ResolvePlanApproval` RPC.

use futures::future::BoxFuture;
use holt_doc::parts::{MessagePart, PlanApprovalState};
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The plan is the card's Markdown, so it is bounded like any model-authored
/// payload — generous enough for any real plan, small enough that a runaway
/// reply cannot flood the transcript.
const MAX_PLAN_CHARS: usize = 32_000;

const DESCRIPTION: &str = "Submit your completed implementation plan for the user's approval. \
Pass the ENTIRE plan as one Markdown string — the approval card shows exactly that Markdown. \
Call it once the plan is decision-complete; settling intent and tradeoffs with the user first \
happens in ordinary text. Each submission fully replaces the previous one. STOP your turn \
after calling — the user approves the plan or replies with feedback, which arrives as the \
next message. Do not also wrap the plan in a <proposed_plan> block.";

fn text_result(text: String, details: serde_json::Value) -> Result<AgentToolResult, String> {
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text,
            ..Default::default()
        })],
        details,
        ..Default::default()
    })
}

fn submit_plan(params: &serde_json::Value) -> Result<AgentToolResult, String> {
    let plan = params
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .ok_or("pass \"plan\" — the complete plan as one Markdown string")?;
    if plan.is_empty() {
        return Err("the plan must be a non-empty Markdown string".into());
    }
    if plan.chars().count() > MAX_PLAN_CHARS {
        return Err(format!(
            "the plan is over the {MAX_PLAN_CHARS}-character cap — split the detail into \
             the workspace exploration and keep the plan to the decisions and their steps"
        ));
    }
    text_result(
        "Plan submitted for approval. STOP this turn — the user approves it or replies \
         with feedback, which arrives as the next message."
            .into(),
        json!({ "plan": plan }),
    )
}

pub(crate) fn create_submit_plan_tool() -> AgentTool {
    AgentTool {
        name: "submit_plan".into(),
        label: "Submit Plan".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The complete plan as ONE Markdown string — exactly what \
        the approval card shows. Each submission replaces the previous one."
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let result = if signal.is_some_and(CancellationToken::is_cancelled) {
                    Err("submit_plan cancelled".into())
                } else {
                    submit_plan(params)
                };
                Box::pin(async move { result })
                    as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// The approval card a settled `submit_plan` call leaves in the transcript,
/// built from its result details — the same landing `ask_user` uses.
pub(crate) fn tool_card(
    tool_call_id: &str,
    tool_name: &str,
    details: &serde_json::Value,
) -> Option<MessagePart> {
    if tool_name != "submit_plan" {
        return None;
    }
    let content = details.get("plan")?.as_str()?.trim();
    (!content.is_empty()).then(|| MessagePart::PlanApproval {
        id: format!("{tool_call_id}-card"),
        content: content.to_string(),
        state: PlanApprovalState::Pending,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_call_carries_the_plan_for_the_card() {
        let result = submit_plan(&json!({ "plan": "  ## Steps\n1. ship it  " })).unwrap();
        assert_eq!(result.details, json!({ "plan": "## Steps\n1. ship it" }));
    }

    #[test]
    fn rejects_missing_empty_and_oversized_plans() {
        assert!(submit_plan(&json!({})).is_err());
        assert!(submit_plan(&json!({ "plan": "   " })).is_err());
        assert!(submit_plan(&json!({ "plan": "x".repeat(MAX_PLAN_CHARS + 1) })).is_err());
        assert!(submit_plan(&json!({ "plan": "x".repeat(MAX_PLAN_CHARS) })).is_ok());
    }

    #[test]
    fn tool_card_builds_only_from_a_submit_plan_result() {
        let details = json!({ "plan": "## The plan" });
        let Some(MessagePart::PlanApproval { id, content, state }) =
            tool_card("t1", "submit_plan", &details)
        else {
            panic!("expected a plan approval card");
        };
        assert_eq!(id, "t1-card");
        assert_eq!(content, "## The plan");
        assert_eq!(state, PlanApprovalState::Pending);
        assert!(tool_card("t1", "bash", &details).is_none());
        assert!(tool_card("t1", "submit_plan", &json!({})).is_none());
        assert!(tool_card("t1", "submit_plan", &json!({ "plan": "  " })).is_none());
    }
}
