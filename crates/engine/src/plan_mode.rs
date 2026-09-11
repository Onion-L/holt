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

use holt_proto::{ActivePlan, Chat, PlanLifecycle};
use pi_core::agent::types::AgentTool;

use crate::agent::AgentRuntime;

/// The workspace-relative directory plan documents live in, under the
/// chat's working directory (`<cwd>/.holt/plans`).
pub(crate) const PLAN_DIR: &str = ".holt/plans";

/// Hard cap on a submitted plan document (chars): approval must stay
/// reviewable, and the plan rides the implementation Turn's model context
/// whole.
pub(crate) const MAX_PLAN_CHARS: usize = 64 * 1024;

/// The model-only corrective continuation (ADR-0025): appended as a user
/// message and run once when a planning Turn ends without `submit_plan`.
pub(crate) const CORRECTIVE_NUDGE: &str = "[Plan Mode] Your turn ended without calling \
`submit_plan`. Ordinary text cannot submit the plan and the user has seen no approval \
request. Finish the plan document with `write_plan` if needed, then call `submit_plan`.";

/// The one planning Turn's plan context: the active revision's id (minted
/// at admission when the chat carries none, kept across the planning
/// cycle's turns and restarts) and the document path the plan tools
/// address. Snapshot at Turn admission — entering or leaving Plan Mode
/// mid-Turn cannot move it.
#[derive(Clone, Debug)]
pub(crate) struct TurnPlan {
    pub(crate) plan_id: String,
    pub(crate) plan_path: PathBuf,
}

/// Mint a fresh revision: a unique plan id per plan and per revision, so
/// documents never overwrite each other and older revisions stay on disk.
/// The revision is persisted on the chat row by the admission path.
pub(crate) fn mint_active_plan() -> ActivePlan {
    ActivePlan {
        plan_id: uuid::Uuid::new_v4().to_string(),
        state: PlanLifecycle::Planning,
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
/// model cannot keep exploring or rewriting after submission.
fn submit_plan_tool(
    runtime: Arc<AgentRuntime>,
    chat_id: String,
    plan: TurnPlan,
    submitted: Arc<AtomicBool>,
) -> AgentTool {
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
            let runtime = Arc::clone(&runtime);
            let chat_id = chat_id.clone();
            let plan = plan.clone();
            Box::pin(async move { submit_plan(&runtime, &chat_id, &plan, submitted).await })
        }),
    }
}

/// The plan tools for one planning Turn: the fixed-path document write and
/// the submission gate that validates the document, persists the
/// awaiting-approval state, and stops the turn.
pub(crate) fn plan_tools(
    plan: &TurnPlan,
    runtime: Arc<AgentRuntime>,
    chat_id: &str,
    submitted: Arc<AtomicBool>,
) -> Vec<AgentTool> {
    vec![
        write_plan_tool(plan.plan_path.clone()),
        submit_plan_tool(runtime, chat_id.to_string(), plan.clone(), submitted),
    ]
}

/// The `submit_plan` execution: read the active revision's document,
/// validate it, persist the awaiting-approval state, and flag the turn for
/// its stop. Any failure is an error tool result the model reads — the
/// planning turn keeps going and nothing awaiting-approval is recorded.
async fn submit_plan(
    runtime: &AgentRuntime,
    chat_id: &str,
    plan: &TurnPlan,
    submitted: Arc<AtomicBool>,
) -> Result<pi_core::agent::types::AgentToolResult, String> {
    let text = std::fs::read_to_string(&plan.plan_path).map_err(|error| {
        format!("could not read the plan document (write it with `write_plan` first): {error}")
    })?;
    if text.trim().is_empty() {
        return Err(
            "The plan document is empty — write the plan with `write_plan` before submitting."
                .into(),
        );
    }
    let chars = text.chars().count();
    if chars > MAX_PLAN_CHARS {
        return Err(format!(
            "The plan document is {chars} characters; the limit is {MAX_PLAN_CHARS}. \
Tighten the plan with `write_plan` before submitting."
        ));
    }
    // The persisted awaiting-approval state: the single fact the
    // ResolveApproval RPC (and the UI card) address. The in-memory state
    // moves first and rolls back if the file write fails.
    let outcome = {
        let mut chats = runtime
            .chats
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) else {
            return Err("this chat no longer exists".into());
        };
        let Some(state) = row.plan_mode.as_mut() else {
            return Err("Plan Mode is no longer active for this chat".into());
        };
        let Some(active) = state
            .active_plan
            .as_mut()
            .filter(|active| active.plan_id == plan.plan_id)
        else {
            return Err("this planning turn's plan revision is no longer the active one".into());
        };
        let previous = std::mem::replace(&mut active.state, PlanLifecycle::AwaitingApproval);
        (
            previous,
            crate::store::persist_chats(&runtime.data_dir, &chats),
        )
    };
    match outcome {
        (_, Ok(())) => {}
        (previous, Err(error)) => {
            let mut chats = runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(row) = chats.iter_mut().find(|row| row.id == chat_id)
                && let Some(state) = row.plan_mode.as_mut()
                && let Some(active) = state.active_plan.as_mut()
                && active.plan_id == plan.plan_id
            {
                active.state = previous;
            }
            return Err(format!("could not record the submission: {error}"));
        }
    }
    runtime.publish_chats();
    submitted.store(true, Ordering::Release);
    Ok(pi_core::agent::types::AgentToolResult {
        content: vec![pi_core::ai::types::BlockContent::Text(
            pi_core::ai::types::TextContent {
                text: "Plan submitted for approval. The planning turn ends here; wait for the \
user's verdict."
                    .into(),
                ..Default::default()
            },
        )],
        details: Default::default(),
        usage: None,
        added_tool_names: None,
        terminate: None,
    })
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
    use crate::agent::AgentRuntime;
    use holt_proto::WorkspaceScope;

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
        let write = write_plan_tool(path.clone());
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
    }

    /// A runtime with one planning chat (`chat-1`) carrying the `p1`
    /// revision in Planning state, over `dir` as the data dir.
    fn planning_runtime(dir: &std::path::Path) -> Arc<AgentRuntime> {
        let chat = holt_proto::Chat {
            id: "chat-1".into(),
            device_id: "device".into(),
            title: None,
            title_source: Default::default(),
            title_task_started: false,
            archived: false,
            cwd: Some(dir.to_string_lossy().into_owned()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: Some(holt_proto::ChatPlanState {
                entry_permission_mode: Default::default(),
                active_plan: Some(ActivePlan {
                    plan_id: "p1".into(),
                    state: PlanLifecycle::Planning,
                }),
            }),
        };
        Arc::new(AgentRuntime::new(
            "device".into(),
            WorkspaceScope::Local,
            dir.to_path_buf(),
            vec![chat],
            None,
        ))
    }

    fn submit_tool(dir: &std::path::Path, path: PathBuf, flag: Arc<AtomicBool>) -> AgentTool {
        submit_plan_tool(
            planning_runtime(dir),
            "chat-1".into(),
            TurnPlan {
                plan_id: "p1".into(),
                plan_path: path,
            },
            flag,
        )
    }

    #[tokio::test]
    async fn submit_plan_rejects_missing_empty_and_oversized_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".holt/plans/c-1-p.md");
        let flag = Arc::new(AtomicBool::new(false));
        let submit = submit_tool(dir.path(), path.clone(), Arc::clone(&flag));

        // Missing document: the model must write before submitting.
        assert!(
            (submit.execute)("call-1", &serde_json::json!({}), None, None)
                .await
                .is_err()
        );
        assert!(!flag.load(Ordering::Acquire));

        // An empty document is not a plan.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "   \n").unwrap();
        assert!(
            (submit.execute)("call-2", &serde_json::json!({}), None, None)
                .await
                .is_err()
        );

        // An oversized document is refused whole: approval stays reviewable.
        std::fs::write(&path, "x".repeat(MAX_PLAN_CHARS + 1)).unwrap();
        let error = (submit.execute)("call-3", &serde_json::json!({}), None, None)
            .await
            .unwrap_err();
        assert!(error.contains("limit is"), "{error}");

        // None of the failures persisted anything: no chats.json exists,
        // so the awaiting-approval flip was never recorded.
        assert!(crate::store::load_chats(dir.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn submit_plan_persists_awaiting_approval_and_flags_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".holt/plans/c-1-p.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# Plan\n- step").unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let submit = submit_tool(dir.path(), path, Arc::clone(&flag));

        (submit.execute)("call-1", &serde_json::json!({}), None, None)
            .await
            .unwrap();
        assert!(flag.load(Ordering::Acquire));

        // The flip is durable: a fresh load of the data dir sees it.
        let chats = crate::store::load_chats(dir.path()).unwrap();
        assert_eq!(
            chats[0]
                .plan_mode
                .as_ref()
                .unwrap()
                .active_plan
                .as_ref()
                .unwrap()
                .state,
            PlanLifecycle::AwaitingApproval
        );
    }

    #[tokio::test]
    async fn submit_plan_refuses_a_revision_that_is_no_longer_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".holt/plans/c-1-p.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# Plan").unwrap();
        // A chat whose active revision is a DIFFERENT id (the turn was
        // admitted, then a resolution/revision replaced it underneath).
        let runtime = planning_runtime(dir.path());
        runtime.chats.write().unwrap()[0]
            .plan_mode
            .as_mut()
            .unwrap()
            .active_plan = Some(ActivePlan {
            plan_id: "other".into(),
            state: PlanLifecycle::Planning,
        });
        let submit = submit_plan_tool(
            Arc::clone(&runtime),
            "chat-1".into(),
            TurnPlan {
                plan_id: "p1".into(),
                plan_path: path,
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(
            (submit.execute)("call-1", &serde_json::json!({}), None, None)
                .await
                .is_err()
        );
        assert_eq!(
            runtime.chats.read().unwrap()[0]
                .plan_mode
                .as_ref()
                .unwrap()
                .active_plan
                .as_ref()
                .unwrap()
                .state,
            PlanLifecycle::Planning
        );
    }

    #[test]
    fn minted_revision_ids_are_unique_per_call() {
        let first = mint_active_plan();
        let second = mint_active_plan();
        assert_ne!(first.plan_id, second.plan_id);
        assert_eq!(first.state, PlanLifecycle::Planning);
    }
}
