//! Plan Mode (ADR-0025): the chat-level planning checkpoint orthogonal to
//! the permission mode. The state rides the chat row (`Chat::plan_mode`,
//! restored by restart); this module owns the path/layout helpers for the
//! plan documents. The planning-turn shaping (tool whitelist, system prompt
//! block, corrective continuation) that the run loop applies to a Turn
//! admitted under Plan Mode grows in alongside the plan lifecycle.

use std::path::PathBuf;

use holt_proto::Chat;

/// The workspace-relative directory plan documents live in, under the
/// chat's working directory (`<cwd>/.holt/plans`).
pub(crate) const PLAN_DIR: &str = ".holt/plans";

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
}
