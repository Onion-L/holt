//! The git surface (ADR-0001/0022/0024): refs, history/fetch, checkout
//! diffs in their modes, and turn change sets.

use holt_proto::TurnChangeSetReply;
use holt_rpc::{RpcError, RpcReply};

use super::{optional_string, required_string};
use crate::EngineService;

impl EngineService {
    // Git capability (ADR-0001): branch listing and safe switching
    // for space folders. Errors carry git's own message — the picker
    // renders it in place.
    pub(super) async fn list_refs(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let refs = self
            .git
            .list_refs(repo_path)
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&refs)
    }

    pub(super) async fn list_branches(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let branches = self
            .git
            .list_branches(repo_path)
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&branches)
    }

    pub(super) async fn switch_ref(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let ref_name = required_string(&params, "refName")?;
        self.git
            .switch_ref(repo_path, ref_name)
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&serde_json::json!({}))
    }

    pub(super) async fn create_branch(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let name = required_string(&params, "name")?;
        let base_ref = params
            .get("baseRef")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        self.git
            .create_branch(repo_path, name, base_ref)
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&serde_json::json!({}))
    }

    // The diff family. Working-tree mode is the live capture;
    // branch mode diffs the merge-base with a chosen base ref;
    // commit mode pins parent → commit without the working tree.
    // The turn mode arrives with its slice.
    pub(super) async fn get_checkout_diff(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let cwd = required_string(&params, "cwd")?;
        let mode = required_string(&params, "mode")?;
        let base_ref = params
            .get("baseRef")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let commit_sha = params
            .get("commitSha")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match mode {
            "commit" => {
                let commit_sha =
                    commit_sha
                        .filter(|sha| !sha.trim().is_empty())
                        .ok_or_else(|| {
                            RpcError::BadParams("commitSha is required for commit diffs".into())
                        })?;
                let diff = self
                    .git
                    .commit_diff(cwd, &self.engine_info.device_id, &commit_sha)
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&diff)
            }
            "turn" => {
                let chat_id = required_string(&params, "chatId")?;
                // No snapshot (never ran, engine restarted) is an
                // explicit error — never a silent empty diff.
                let Some(record) = self.turn_changes.snapshot(chat_id) else {
                    return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                };
                let diff = self
                    .git
                    .turn_diff(
                        cwd,
                        &self.engine_info.device_id,
                        &record.baseline,
                        Some(&record.attribution.snapshot()),
                    )
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&diff)
            }
            "workingTree" | "branch" => {
                let diff = self
                    .git
                    .capture(cwd, &self.engine_info.device_id, mode, base_ref.as_deref())
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&diff)
            }
            other => Err(RpcError::Failed(format!(
                "{other} diffs are not available yet"
            ))),
        }
    }

    pub(super) async fn get_checkout_file_diff_text(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let request: holt_proto::GetCheckoutFileDiffTextRequest = holt_rpc::parse_params(params)?;
        match request.mode.as_str() {
            "turn" => {
                let Some(chat_id) = request.chat_id.as_deref().filter(|id| !id.is_empty()) else {
                    return Err(RpcError::BadParams(
                        "chatId is required for turn diffs".into(),
                    ));
                };
                // A Turn-addressed read (ADR-0024 ticket 02) serves
                // the settled Turn's immutable before/after pair
                // from its persisted record: restarts and later
                // workspace edits cannot move history. Without a
                // record — the Turn still runs, or its write failed —
                // only the chat's CURRENT Turn may fall through to
                // the live baseline; an older Turn has no reviewable
                // pair to invent.
                let addressed = request
                    .message_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                if let Some(message_id) = addressed {
                    if let Some(record) =
                        crate::turn_change_store::load(&self.data_dir, chat_id, message_id)
                    {
                        let Some(content) = record.content_for(&request.path) else {
                            return Err(RpcError::Failed(format!(
                                "{} is not part of that turn's changes",
                                request.path
                            )));
                        };
                        return RpcReply::value(&holt_proto::CheckoutFileDiffText {
                            diff_checksum: request.diff_checksum.clone(),
                            old_text: content.old_text.clone(),
                            new_text: content.new_text.clone(),
                            old_content_hash: content.old_content_hash.clone(),
                            new_content_hash: content.new_content_hash.clone(),
                            binary: content.binary,
                            truncated: content.truncated,
                            stale: false,
                        });
                    }
                    let is_current = self
                        .turn_changes
                        .snapshot(chat_id)
                        .is_some_and(|record| record.message_id == message_id);
                    if !is_current {
                        return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                    }
                }
                let Some(record) = self.turn_changes.snapshot(chat_id) else {
                    return Err(RpcError::Failed(NO_TURN_RECORDED.into()));
                };
                let text = self
                    .git
                    .turn_file_text(
                        &request.cwd,
                        &self.engine_info.device_id,
                        &request,
                        &record.baseline,
                        Some(&record.attribution.snapshot()),
                    )
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&text)
            }
            "" | "workingTree" | "branch" | "commit" => {
                let text = self
                    .git
                    .capture_file_text(&request.cwd, &self.engine_info.device_id, &request)
                    .await
                    .map_err(git_fault)?;
                RpcReply::value(&text)
            }
            other => Err(RpcError::Failed(format!(
                "{other} diffs are not available yet"
            ))),
        }
    }

    // Turn change sets (ADR-0024): the net Git change from a Turn's
    // admission baseline to its live or final working tree. Separate
    // from the checkout-diff scopes, which the UI's Changes pane
    // owns; this family feeds the Turn card. With `messageId` the
    // read addresses one specific Turn — the in-memory record while
    // the engine knows it, else the persisted record a restart
    // restores (ticket 02).
    pub(super) async fn get_turn_change_set(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?;
        let root = self.turn_change_root(chat_id)?;
        let message_id = optional_string(&params, "messageId");
        let Some(message_id) = message_id else {
            // The chat's current Turn is a live Git read: the non-Git
            // answer keys on the working directory, never on a
            // capture error.
            if !self.git.is_work_tree(&root).await {
                return RpcReply::value(&TurnChangeSetReply::Unsupported {
                    reason: NON_GIT_CHANGE_SET_REASON.into(),
                });
            }
            return match self
                .turn_changes
                .read(&self.git, &self.engine_info.device_id, chat_id)
                .await
                .map_err(git_fault)?
            {
                Some(change_set) => RpcReply::value(&TurnChangeSetReply::Captured(change_set)),
                None => Err(RpcError::Failed(NO_TURN_RECORDED.into())),
            };
        };
        // Memory first (a live Turn, or the frozen current/last
        // one); a restart — or a working tree that can no longer be
        // captured — falls back to the persisted record, which
        // outlives the repository it came from: only a Turn with no
        // record anywhere answers by its working directory.
        let memory = self
            .turn_changes
            .read_message(&self.git, &self.engine_info.device_id, chat_id, &message_id)
            .await;
        if let Ok(Some(change_set)) = memory {
            return RpcReply::value(&TurnChangeSetReply::Captured(change_set));
        }
        if let Some(record) = crate::turn_change_store::load(&self.data_dir, chat_id, &message_id) {
            return RpcReply::value(&TurnChangeSetReply::Captured(record.change_set(chat_id)));
        }
        if !self.git.is_work_tree(&root).await {
            return RpcReply::value(&TurnChangeSetReply::Unsupported {
                reason: NON_GIT_CHANGE_SET_REASON.into(),
            });
        }
        match memory {
            Err(fault) => Err(git_fault(fault)),
            Ok(_) => Err(RpcError::Failed(NO_TURN_RECORDED.into())),
        }
    }

    pub(super) async fn watch_turn_change_set(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let chat_id = required_string(&params, "chatId")?.to_string();
        let root = self.turn_change_root(&chat_id)?;
        if !self.git.is_work_tree(&root).await {
            use futures::StreamExt;
            let value = serde_json::to_value(TurnChangeSetReply::Unsupported {
                reason: NON_GIT_CHANGE_SET_REASON.into(),
            })
            .map_err(|error| RpcError::Failed(format!("serialize response: {error}")))?;
            return Ok(RpcReply::Stream(futures::stream::iter([value]).boxed()));
        }
        let stream = crate::turn_change_watch::subscribe(
            std::path::PathBuf::from(root),
            chat_id,
            self.git.clone(),
            self.engine_info.device_id.clone(),
            self.turn_changes.clone(),
            self.turn_events.clone(),
        )
        .map_err(RpcError::Failed)?;
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    // History: the topologically ordered commit graph with refs,
    // paged by cursor, plus the fetch action that updates
    // remote-tracking refs without touching any checkout state.
    pub(super) async fn list_git_history(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let cwd = required_string(&params, "cwd")?;
        let cursor = params
            .get("cursor")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let limit = params
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let page = self
            .git
            .history(cwd, cursor, limit)
            .await
            .map_err(RpcError::Failed)?;
        RpcReply::value(&page)
    }

    pub(super) async fn fetch_all(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.git.fetch_all(repo_path),
        )
        .await
        .map_err(|_| RpcError::Failed("fetch timed out after 30s".into()))?
        .map_err(RpcError::Failed)?;
        RpcReply::value(&serde_json::json!({}))
    }
}

/// The explicit non-Git answer `GetTurnChangeSet`/`WatchTurnChangeSet`
/// share (ADR-0024): an empty change set must never stand in for it.
const NON_GIT_CHANGE_SET_REASON: &str = "the chat's working directory is not a Git work tree";

/// The turn-diff scopes' soft-matchable phrase for a chat whose current Turn
/// has no recorded baseline (never ran, engine restarted).
const NO_TURN_RECORDED: &str = "no turn recorded for this chat yet";

/// Report git capture faults at the right RPC severity: caller-input
/// problems are bad params, repository failures are opaque errors.
fn git_fault(fault: crate::git::GitFault) -> RpcError {
    match fault {
        crate::git::GitFault::BadParams(message) => RpcError::BadParams(message),
        crate::git::GitFault::Error(message) => RpcError::Failed(message),
    }
}
