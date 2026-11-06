//! The integration checks RPC slice: run, watch, stop, and the per-check
//! live log. The executor lives in `integration_checks`; the git work
//! (merge tree, throwaway checkout) in `git.rs`.

use holt_rpc::{RpcError, RpcReply};

use super::required_string;
use crate::EngineService;

impl EngineService {
    pub(super) async fn run_checks(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let worktree = required_string(&params, "worktree")?.to_string();
        let trunk = required_string(&params, "trunk")?.to_string();
        let shell = crate::shell_env::agent_shell();
        let shell_groups = shell_groups_dir(&self.data_dir);
        let run_id = self
            .checks
            .run(
                self.git.clone(),
                self.data_dir.clone(),
                shell,
                shell_groups,
                repo_path.to_string(),
                worktree,
                trunk,
            )
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&serde_json::json!({ "runId": run_id }))
    }

    pub(super) async fn watch_checks(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let worktree = required_string(&params, "worktree")?.to_string();
        let checkout_key = self
            .git
            .checkout_key(repo_path)
            .await
            .map_err(RpcError::Failed)?;
        let receiver = self.checks.watch(&self.data_dir, &checkout_key, &worktree);
        // Staleness is judged at watch time, not persisted: each frame
        // compares the run's recorded heads with the repository's current
        // ones (a live run goes stale the same way a persisted one does).
        let git = self.git.clone();
        let repo_path = repo_path.to_string();
        let stream = futures::stream::unfold(
            (receiver, true, git, repo_path, worktree),
            |(mut receiver, first, git, repo_path, worktree)| async move {
                if !first && receiver.changed().await.is_err() {
                    return None;
                }
                let value = receiver.borrow_and_update().clone();
                let value = match value {
                    Some(mut run) => {
                        if !run.trunk.is_empty()
                            && let Ok((trunk_head, branch_head)) = git
                                .worktree_and_trunk_heads(&repo_path, &worktree, &run.trunk)
                                .await
                        {
                            run.stale =
                                trunk_head != run.trunk_head || branch_head != run.branch_head;
                        }
                        serde_json::to_value(run).ok()?
                    }
                    None => serde_json::Value::Null,
                };
                Some((value, (receiver, false, git, repo_path, worktree)))
            },
        );
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    pub(super) fn stop_checks(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let run_id = required_string(&params, "runId")?;
        self.checks.stop(run_id);
        RpcReply::value(&serde_json::json!({}))
    }

    /// One check's live log — the same `TerminalEvent` stream the terminal
    /// and background-process surfaces serve.
    pub(super) async fn subscribe_check_log(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            run_id: String,
            name: String,
            #[serde(default)]
            after_seq: u64,
        }
        let request: Request = holt_rpc::parse_params(params)?;
        let process = self
            .checks
            .live_check_process(&request.run_id, &request.name)
            .ok_or_else(|| {
                RpcError::Failed(format!(
                    "check {} of run {} is not live",
                    request.name, request.run_id
                ))
            })?;
        Ok(process.subscribe_events(request.after_seq))
    }
}

/// The crash-reap registry directory, unix only (see the agent path).
fn shell_groups_dir(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        Some(crate::shell_groups::registry_dir(data_dir))
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        None
    }
}
