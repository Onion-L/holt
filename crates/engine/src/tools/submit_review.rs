//! The review Turn's structured submission channel: `submit_review` hands
//! the findings to the chat row as typed data and ENDS the Turn
//! (`terminate: true` — unlike `submit_plan`, a review has nothing more to
//! do once submitted; the prompt's "no submission by Turn's end marks the
//! review incomplete" makes a lingering Turn the failure shape).

use std::sync::Arc;

use futures::future::BoxFuture;
use holt_core::{
    agent::types::{AgentTool, AgentToolResult},
    ai::types::{BlockContent, TextContent},
};
use holt_doc::parts::MessagePart;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentRuntime;

const MAX_FINDINGS: usize = 50;
const MAX_TEXT_CHARS: usize = 2_000;

const DESCRIPTION: &str = "Submit your review of the branch diff. Call it exactly once, when the \
review is complete — a successful call ends your turn. verdict is \"clean\" (no problems) or \
\"issues\". Each finding needs a severity (high|medium|low), the file path, the line in the \
diff, a short title, the evidence in the detail, and an origin: \"introduced\" for problems \
this diff creates, \"pre-existing\" for older ones, \"unknown\" when you cannot tell. With \
verdict \"clean\" the findings list must be empty.";

/// Where the submit lands: the runtime's chat registry and the reviewed
/// chat's id.
#[derive(Clone)]
pub(crate) struct ReviewSink {
    pub runtime: Arc<AgentRuntime>,
    pub chat_id: String,
}

fn parse_severity(value: &str) -> Result<(), String> {
    matches!(value, "high" | "medium" | "low")
        .then_some(())
        .ok_or_else(|| format!("severity must be high, medium, or low — got {value:?}"))
}

fn parse_origin(value: &str) -> Result<(), String> {
    matches!(value, "introduced" | "pre-existing" | "unknown")
        .then_some(())
        .ok_or_else(|| {
            format!("origin must be introduced, pre-existing, or unknown — got {value:?}")
        })
}

fn bounded(value: &str, field: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if trimmed.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "{field} is over the {MAX_TEXT_CHARS}-character cap"
        ));
    }
    Ok(trimmed.to_string())
}

fn submit_review(sink: &ReviewSink, params: &serde_json::Value) -> Result<AgentToolResult, String> {
    let verdict = params
        .get("verdict")
        .and_then(serde_json::Value::as_str)
        .ok_or("pass \"verdict\" — \"clean\" or \"issues\"")?;
    if !matches!(verdict, "clean" | "issues") {
        return Err(format!(
            "verdict must be \"clean\" or \"issues\" — got {verdict:?}"
        ));
    }
    let empty = Vec::new();
    let raw_findings = params
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&empty);
    if verdict == "clean" && !raw_findings.is_empty() {
        return Err("a \"clean\" verdict carries no findings".into());
    }
    if verdict == "issues" && raw_findings.is_empty() {
        return Err("an \"issues\" verdict needs at least one finding".into());
    }
    if raw_findings.len() > MAX_FINDINGS {
        return Err(format!(
            "over the {MAX_FINDINGS}-finding cap — report the significant problems"
        ));
    }
    let mut findings = Vec::with_capacity(raw_findings.len());
    for raw in raw_findings {
        let severity = raw
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .ok_or("each finding needs a severity (high|medium|low)")?;
        parse_severity(severity)?;
        let path = bounded(
            raw.get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            "path",
        )?;
        let line = raw
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .filter(|line| *line > 0 && *line <= u32::MAX as u64)
            .map(|line| line as u32);
        let title = bounded(
            raw.get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            "title",
        )?;
        let detail = raw
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if detail.chars().count() > MAX_TEXT_CHARS {
            return Err("detail is over the character cap".into());
        }
        let origin = raw
            .get("origin")
            .and_then(serde_json::Value::as_str)
            .ok_or("each finding needs an origin (introduced|pre-existing|unknown)")?;
        parse_origin(origin)?;
        findings.push(holt_proto::ReviewFinding {
            id: uuid::Uuid::new_v4().to_string(),
            severity: severity.to_string(),
            path,
            line,
            title,
            detail,
            origin: origin.to_string(),
        });
    }
    // Land on the chat row: the review state gains its findings and verdict.
    {
        let mut chats = sink
            .runtime
            .chats
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(row) = chats.iter_mut().find(|row| row.id == sink.chat_id)
            && let Some(review) = &mut row.review
        {
            review.findings = Some(findings.clone());
            review.verdict = Some(verdict.to_string());
        }
        drop(chats);
        if let Err(error) = sink.runtime.persist_chats_locked() {
            // Not submitted: the disk write failed, so the verdict would
            // vanish on restart. Roll the row back so it never claims a
            // submission, and keep the Turn open — the reviewer can retry —
            // instead of terminating over a silent loss.
            tracing::warn!(%error, chat_id = %sink.chat_id, "could not persist the review");
            let mut chats = sink
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(row) = chats.iter_mut().find(|row| row.id == sink.chat_id)
                && let Some(review) = &mut row.review
            {
                review.findings = None;
                review.verdict = None;
            }
            drop(chats);
            return Err(format!(
                "could not persist the review ({error}); the submission did not stick — \
                 submit again"
            ));
        }
        sink.runtime.publish_chats();
    }
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: "Review submitted. The turn ends here.".into(),
            ..Default::default()
        })],
        details: json!({ "verdict": verdict, "findings": findings }),
        terminate: Some(true),
        ..Default::default()
    })
}

pub(crate) fn create_submit_review_tool(sink: ReviewSink) -> AgentTool {
    AgentTool {
        name: "submit_review".into(),
        label: "Submit review".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["clean", "issues"] },
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "severity": { "type": "string", "enum": ["high", "medium", "low"] },
                            "path": { "type": "string" },
                            "line": { "type": "integer", "minimum": 1 },
                            "title": { "type": "string" },
                            "detail": { "type": "string" },
                            "origin": { "type": "string", "enum": ["introduced", "pre-existing", "unknown"] }
                        },
                        "required": ["severity", "path", "title", "origin"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["verdict"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |_, params, signal, _| {
            let sink = sink.clone();
            let params = params.clone();
            let cancelled = signal.is_some_and(CancellationToken::is_cancelled);
            Box::pin(async move {
                if cancelled {
                    return Err("submit_review cancelled".into());
                }
                submit_review(&sink, &params)
            }) as BoxFuture<'static, Result<AgentToolResult, String>>
        }),
    }
}

/// The findings card a settled `submit_review` call leaves in the
/// transcript, from its result details.
pub(crate) fn tool_card(
    tool_call_id: &str,
    tool_name: &str,
    details: &serde_json::Value,
) -> Option<MessagePart> {
    if tool_name != "submit_review" {
        return None;
    }
    let verdict = details.get("verdict")?.as_str()?.to_string();
    let findings: Vec<holt_proto::ReviewFinding> = details
        .get("findings")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|finding| serde_json::from_value(finding.clone()).ok())
        .collect();
    Some(MessagePart::ReviewSubmitted {
        id: format!("{tool_call_id}-card"),
        verdict,
        findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink_with_review() -> ReviewSink {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime = crate::agent::AgentRuntime::new(
            "device".into(),
            holt_proto::WorkspaceScope::Local,
            dir.path().to_path_buf(),
            vec![
                serde_json::from_value(serde_json::json!({
                    "id": "review-1",
                    "deviceId": "device",
                    "archived": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "review": {
                        "repoPath": "/repo",
                        "worktree": "wt",
                        "trunk": "main",
                        "trunkHead": "t",
                        "branchHead": "b",
                        "reviewer": "bugbot",
                    },
                }))
                .unwrap(),
            ],
            None,
            crate::mcp::McpPool::load(dir.path()).unwrap(),
            None,
        );
        ReviewSink {
            runtime: std::sync::Arc::new(runtime),
            chat_id: "review-1".into(),
        }
    }

    #[test]
    fn a_clean_verdict_rejects_findings() {
        let error = submit_review(
            &sink_with_review(),
            &json!({ "verdict": "clean", "findings": [ { "severity": "low", "path": "a.rs", "title": "x", "origin": "introduced" } ] }),
        )
        .unwrap_err();
        assert!(error.contains("clean"));
    }

    #[test]
    fn bad_severity_and_origin_are_rejected() {
        let sink = sink_with_review();
        let error = submit_review(
            &sink,
            &json!({ "verdict": "issues", "findings": [ { "severity": "critical", "path": "a.rs", "title": "x", "origin": "introduced" } ] }),
        )
        .unwrap_err();
        assert!(error.contains("severity"));
        let error = submit_review(
            &sink,
            &json!({ "verdict": "issues", "findings": [ { "severity": "low", "path": "a.rs", "title": "x", "origin": "new" } ] }),
        )
        .unwrap_err();
        assert!(error.contains("origin"));
    }
}
