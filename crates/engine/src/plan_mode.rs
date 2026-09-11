//! Plan Mode (ADR-0025): the chat-level planning checkpoint orthogonal to
//! the permission mode. The state rides the chat row (`Chat::plan_mode`,
//! restored by restart); this module owns the plan-document layout helpers
//! and the planning-turn shaping the run loop applies to a Turn admitted
//! under Plan Mode: the read-only tool whitelist, the strong submit
//! requirement in the system prompt, the two plan tools, and the one-shot
//! corrective continuation for a Turn that ends without a submission.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use holt_proto::Chat;
use pi_core::agent::types::AgentTool;

/// The workspace-relative directory plan documents live in, under the
/// chat's working directory (`<cwd>/.holt/plans`).
pub(crate) const PLAN_DIR: &str = ".holt/plans";

/// The model-only corrective continuation (ADR-0025): appended as a user
/// message and run once when a planning Turn ends without `submit_plan`.
pub(crate) const CORRECTIVE_NUDGE: &str = "[Plan Mode] Your turn ended without calling \
`submit_plan`. Ordinary text cannot submit the plan and the user has seen no approval \
request. Finish the plan document with `write_plan` if needed, then call `submit_plan`.";

/// The one planning Turn's plan context: a fresh revision id per planning
/// cycle and the document path the plan tools address. Snapshot at Turn
/// admission — entering or leaving Plan Mode mid-Turn cannot move it.
#[derive(Clone, Debug)]
pub(crate) struct TurnPlan {
    pub(crate) plan_path: PathBuf,
}

/// Mint the planning Turn's revision: a unique plan id per plan and per
/// revision, so documents never overwrite each other and older revisions
/// stay on disk.
pub(crate) fn mint_turn_plan(cwd: &str, chat_id: &str) -> TurnPlan {
    let plan_id = uuid::Uuid::new_v4().to_string();
    TurnPlan {
        plan_path: plan_path(cwd, chat_id, &plan_id),
    }
}

/// The shared read-only tool predicate (ADR-0025): the exploration surface
/// an Explorer subagent gets (ADR-0023) and a planning Turn keeps — nothing
/// that can change files or execute commands. The plan tools ride beside it
/// in Plan Mode, appended separately.
pub(crate) fn read_only_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "read" | "grep" | "read_chat" | "web_fetch" | "web_search"
    )
}

/// The planning system-prompt block: the strong runtime requirement that
/// submission happens ONLY through the `submit_plan` tool call — ordinary
/// text can never trigger approval (ADR-0025).
pub(crate) fn planning_system_block(plan_path: &Path) -> String {
    format!(
        "## Plan Mode (active)\n\n\
This turn is a PLANNING turn. Explore the workspace and produce an \
implementation plan for the user to review and approve. You must not \
change the workspace.\n\n\
- Your tools are read-only exploration (`read`, `grep`, `read_chat`, \
`web_fetch`, and `web_search` when available) plus exactly two plan \
tools: `write_plan` and `submit_plan`. `bash`, `write`, `edit`, and \
subagent delegation are unavailable; do not attempt workarounds.\n\
- Write the complete plan document with `write_plan`. It writes exactly \
`{}` — the Markdown document the user reviews. Rewrite it as often as \
your exploration changes the plan.\n\
- When the plan document is complete you MUST call `submit_plan`. \
Submission happens ONLY through the `submit_plan` tool call. Ending the \
turn with ordinary text does NOT submit the plan, does NOT reach \
approval, and is a protocol violation.\n",
        plan_path.display()
    )
}

/// The `write_plan` tool: the one write path Plan Mode permits, fixed to
/// the active revision's document — the tool takes content only, so the
/// model cannot aim it anywhere else.
fn write_plan_tool(plan_path: PathBuf) -> AgentTool {
    AgentTool {
        name: "write_plan".into(),
        label: "Write plan".into(),
        description: "Write or replace the complete plan document for this planning session. \
Takes the full Markdown content and writes it to the plan document the user reviews. \
Keep the plan concrete: goal, approach, files to change, verification steps, and open \
questions. Rewrite the whole document each time — partial edits are not supported."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "minLength": 1}
            },
            "required": ["content"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |_id, args, _signal, _update| {
            let plan_path = plan_path.clone();
            let args = args.clone();
            Box::pin(async move {
                let content = args
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .filter(|content| !content.trim().is_empty())
                    .ok_or("content is required")?;
                if let Some(parent) = plan_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("could not create the plan directory: {error}"))?;
                }
                std::fs::write(&plan_path, content)
                    .map_err(|error| format!("could not write the plan document: {error}"))?;
                Ok(pi_core::agent::types::AgentToolResult {
                    content: vec![pi_core::ai::types::BlockContent::Text(
                        pi_core::ai::types::TextContent {
                            text: format!("Plan document written to {}.", plan_path.display()),
                            ..Default::default()
                        },
                    )],
                    details: Default::default(),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// The `submit_plan` tool: the only submission channel (ADR-0025). It sets
/// the flag the loop's `should_stop_after_turn` hook watches, so the
/// planning Turn ends at the close of the tool round that submitted — the
/// model cannot keep exploring or rewriting after submission. Issue 03
/// adds the document validation and the persisted awaiting-approval state.
fn submit_plan_tool(submitted: Arc<AtomicBool>) -> AgentTool {
    AgentTool {
        name: "submit_plan".into(),
        label: "Submit plan".into(),
        description: "Submit the plan document for user approval and end the planning turn. \
Call this only after the plan document is written with write_plan. There are no \
arguments; the plan itself lives in the document, not in this call."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |_id, _args, _signal, _update| {
            let submitted = Arc::clone(&submitted);
            Box::pin(async move {
                submitted.store(true, Ordering::Release);
                Ok(pi_core::agent::types::AgentToolResult {
                    content: vec![pi_core::ai::types::BlockContent::Text(
                        pi_core::ai::types::TextContent {
                            text: "Plan submitted for approval. The planning turn ends here; \
wait for the user's verdict."
                                .into(),
                            ..Default::default()
                        },
                    )],
                    details: Default::default(),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }),
    }
}

/// The plan tools for one planning Turn: the fixed-path document write and
/// the submission flag the loop stop hook watches.
pub(crate) fn plan_tools(plan: &TurnPlan, submitted: Arc<AtomicBool>) -> Vec<AgentTool> {
    vec![
        write_plan_tool(plan.plan_path.clone()),
        submit_plan_tool(submitted),
    ]
}

/// The chat-id stem for plan file names: path-safe ids pass through, and
/// anything else (legacy ids may carry arbitrary text) collapses to its
/// safe characters — collisions are impossible in practice because the
/// unique plan id still separates the files.
pub(crate) fn chat_file_stem(chat_id: &str) -> String {
    if crate::store::id_is_path_safe(chat_id) {
        return chat_id.to_string();
    }
    let collapsed: String = chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    collapsed.trim_matches('-').to_string()
}

/// The document for one plan revision: `<cwd>/.holt/plans/<chat>-<plan>.md`.
pub(crate) fn plan_path(cwd: &str, chat_id: &str, plan_id: &str) -> PathBuf {
    PathBuf::from(cwd)
        .join(PLAN_DIR)
        .join(format!("{}-{plan_id}.md", chat_file_stem(chat_id)))
}

/// The active plan's resolved document, when the chat is planning, carries a
/// revision, and has a working directory to resolve it against.
pub(crate) fn active_plan_path(chat: &Chat) -> Option<PathBuf> {
    let plan = chat.plan_mode.as_ref()?.active_plan.as_ref()?;
    let plan_id = &plan.plan_id;
    let cwd = chat.cwd.as_deref()?;
    Some(plan_path(
        &crate::local_fs::expand_tilde(cwd),
        &chat.id,
        plan_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_paths_live_under_the_cwd_holt_plans_dir() {
        let path = plan_path("/repo", "chat-1", "abc");
        assert_eq!(path, PathBuf::from("/repo/.holt/plans/chat-1-abc.md"));
    }

    #[test]
    fn unsafe_chat_ids_collapse_to_a_path_safe_stem() {
        assert_eq!(chat_file_stem("chat-1"), "chat-1");
        assert_eq!(chat_file_stem("../../etc/passwd"), "etc-passwd");
        assert_eq!(chat_file_stem("a/b\\c d"), "a-b-c-d");
        let path = plan_path("/repo", "a/b", "p1");
        assert_eq!(path, PathBuf::from("/repo/.holt/plans/a-b-p1.md"));
    }

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
    fn the_planning_block_names_the_document_and_the_only_submission_channel() {
        let block = planning_system_block(Path::new("/repo/.holt/plans/c-1-p.md"));
        assert!(block.contains("## Plan Mode (active)"));
        assert!(block.contains("/repo/.holt/plans/c-1-p.md"));
        assert!(block.contains("`submit_plan`"));
        assert!(block.contains("ONLY through the `submit_plan` tool call"));
        assert!(block.contains("does NOT submit the plan"));
    }

    #[tokio::test]
    async fn write_plan_writes_only_its_fixed_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".holt/plans/c-1-p.md");
        let tools = plan_tools(
            &TurnPlan {
                plan_path: path.clone(),
            },
            Arc::new(AtomicBool::new(false)),
        );
        let write = tools.iter().find(|tool| tool.name == "write_plan").unwrap();
        let ok = (write.execute)(
            "call-1",
            &serde_json::json!({ "content": "# Plan\n- step" }),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n- step");
        let text = match ok.content.first().unwrap() {
            pi_core::ai::types::BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains(&path.display().to_string()));

        // Empty content is refused and writes nothing.
        assert!(
            (write.execute)("call-2", &serde_json::json!({}), None, None)
                .await
                .is_err()
        );

        // The submit tool has no file access and flips the flag only.
        let flag = Arc::new(AtomicBool::new(false));
        let tools = plan_tools(
            &TurnPlan {
                plan_path: path.clone(),
            },
            Arc::clone(&flag),
        );
        let submit = tools
            .iter()
            .find(|tool| tool.name == "submit_plan")
            .unwrap();
        (submit.execute)("call-3", &serde_json::json!({}), None, None)
            .await
            .unwrap();
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn minted_revision_ids_are_unique_per_call() {
        let first = mint_turn_plan("/repo", "chat-1");
        let second = mint_turn_plan("/repo", "chat-1");
        assert_ne!(first.plan_path, second.plan_path);
    }
}
