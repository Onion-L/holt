//! The integration checks RPC slice: run, watch, stop, and the per-check
//! live log. The executor lives in `integration_checks`; the git work
//! (merge tree, throwaway checkout) in `git.rs`.

use holt_rpc::{RpcError, RpcReply};

use super::{optional_string, required_string};
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

/// The review slice: the reviewer catalog, starting a review (a fresh chat
/// carrying review state), and the review list. The review Turn's toolset
/// shaping lives in `agent.rs`; `submit_review` in `tools/submit_review.rs`.
impl EngineService {
    /// The reviewer catalog: the two built-ins plus the trunk's
    /// `.holt/reviewers/<name>.md` (a repo file shadows its builtin twin).
    pub(super) async fn list_reviewers(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let trunk = required_string(&params, "trunk")?.to_string();
        let mut reviewers = vec![
            ReviewerRow {
                name: "bugbot".into(),
                source: "builtin".into(),
            },
            ReviewerRow {
                name: "security".into(),
                source: "builtin".into(),
            },
        ];
        if let Some(names) = self
            .git
            .reviewers_in_trunk(repo_path, &trunk)
            .await
            .map_err(RpcError::Failed)?
        {
            for name in names {
                if let Some(row) = reviewers.iter_mut().find(|row| row.name == name) {
                    row.source = "repo".into();
                } else {
                    reviewers.push(ReviewerRow {
                        name,
                        source: "repo".into(),
                    });
                }
            }
        }
        reviewers.sort_by(|a, b| a.name.cmp(&b.name));
        RpcReply::value(&reviewers)
    }

    /// Start a review: a fresh chat in the worktree's Space carrying review
    /// state, whose first message (engine-generated) is the diff and its
    /// metadata — never the author conversation.
    pub(super) async fn start_review(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let worktree = required_string(&params, "worktree")?.to_string();
        let trunk = required_string(&params, "trunk")?.to_string();
        let reviewer = required_string(&params, "reviewer")?.to_string();
        let custom_prompt = optional_string(&params, "customPrompt");
        let model_override = optional_string(&params, "model");

        // The reviewed pair and the branch diff (merge-base → workdir).
        let heads = self
            .git
            .worktree_and_trunk_heads(repo_path, &worktree, &trunk)
            .await
            .map_err(RpcError::Failed)?;
        let (trunk_head, branch_head) = heads.clone();
        let worktree_path = self.worktree_path(repo_path, &worktree).await?;
        let diff = self
            .git
            .capture(
                &worktree_path,
                &self.engine_info.device_id,
                "branch",
                Some(&trunk),
            )
            .await
            .map_err(crate::rpc::git_surface::git_fault)?;

        // Resolve the reviewer's prompt: the custom text, else the trunk's
        // override, else the builtin.
        let (reviewer_name, prompt) = match custom_prompt {
            Some(prompt) => ("custom".to_string(), prompt),
            None => {
                let builtin = match reviewer.as_str() {
                    "security" => include_str!("../reviewers/security.md").to_string(),
                    _ => include_str!("../reviewers/bugbot.md").to_string(),
                };
                match self
                    .git
                    .reviewer_prompt(repo_path, &trunk, &reviewer)
                    .await
                    .map_err(RpcError::Failed)?
                {
                    Some(text) => (reviewer.clone(), text),
                    None => (reviewer.clone(), builtin),
                }
            }
        };

        // The author chat (the worktree's own, when there is one): its
        // title rides the first message, its config the default model.
        let author_chat = {
            let chats = self
                .runtime
                .chats
                .read()
                .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
            chats.iter().find(|row| row.id == worktree).cloned()
        };
        let space = self
            .ensure_review_space(&worktree_path)
            .map_err(RpcError::Failed)?;

        // The catalog's model spelling is `provider/model`; the config
        // carries it whole (provider rides alongside for resolution).
        let config = match model_override {
            Some(model) => {
                let provider = model
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
                    .ok_or_else(|| RpcError::BadParams("model must be provider/model".into()))?;
                holt_proto::ChatConfig {
                    provider: provider.into(),
                    model,
                    ..self.default_review_config(&author_chat)
                }
            }
            None => self.default_review_config(&author_chat),
        };

        let chat_id = uuid::Uuid::new_v4().to_string();
        let title = format!("{worktree} · {reviewer_name}");
        let chat = holt_proto::Chat {
            id: chat_id.clone(),
            device_id: self.engine_info.device_id.clone(),
            title: Some(title),
            title_source: holt_proto::TitleSource::UserManual,
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: Some(worktree_path.clone()),
            branch: None,
            checkout_id: space.checkout_id.clone(),
            source_context: None,
            config: Some(config.clone()),
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            space_id: Some(space.id.clone()),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            provider_mode: false,
            worktree: None,
            routine_run: None,
            goal: None,
            review: Some(holt_proto::ChatReviewState {
                repo_path: repo_path.to_string(),
                worktree: worktree.clone(),
                trunk: trunk.clone(),
                trunk_head,
                branch_head,
                reviewer: reviewer_name,
                prompt,
                findings: None,
                verdict: None,
            }),
        };
        {
            let mut chats = self
                .runtime
                .chats
                .write()
                .unwrap_or_else(|error| error.into_inner());
            chats.push(chat);
        }
        self.runtime
            .persist_chats_locked()
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        self.runtime.publish_chats();

        // The engine-generated first message: metadata plus the diff — the
        // author conversation never rides along.
        let author_title = author_chat
            .as_ref()
            .and_then(|chat| chat.title.clone())
            .unwrap_or_default();
        let mut prompt = format!(
            "Review the diff of worktree `{worktree}` against `{trunk}`.\n\n\
             Trunk head: {trunk_head}\n\
             Branch head: {branch_head}\n",
            trunk_head = heads.0,
            branch_head = heads.1,
        );
        if !author_title.is_empty() {
            prompt.push_str(&format!("Author chat: {author_title}\n"));
        }
        prompt.push_str("\nRead the surrounding code as needed. Report only what this diff introduces; submit your findings with `submit_review`.\n\n");
        if diff.truncated {
            prompt.push_str(
                "The diff below was truncated; read the remaining changed files yourself.\n\n",
            );
        }
        prompt.push_str("```diff\n");
        prompt.push_str(&diff.patch);
        prompt.push_str("\n```");

        let chat = self.runtime.chat(&chat_id);
        let request = Self::queued_run_request(&config, &prompt, worktree_path);
        self.enqueue_run(chat, request, uuid::Uuid::new_v4().to_string())?;
        RpcReply::value(&serde_json::json!({ "chatId": chat_id }))
    }

    /// The reviews for one worktree, newest first.
    pub(super) async fn list_reviews(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let repo_path = required_string(&params, "repoPath")?;
        let worktree = required_string(&params, "worktree")?.to_string();
        let trunk = optional_string(&params, "trunk").unwrap_or_else(|| "main".to_string());
        let heads = self
            .git
            .worktree_and_trunk_heads(repo_path, &worktree, &trunk)
            .await;
        let chats = self
            .runtime
            .chats
            .read()
            .map_err(|_| RpcError::Failed("chats lock poisoned".into()))?;
        let sessions: Vec<holt_proto::Session> =
            serde_json::from_value(self.runtime.sessions_tx.subscribe().borrow().clone())
                .unwrap_or_default();
        let mut rows: Vec<serde_json::Value> = chats
            .iter()
            .filter_map(|chat| {
                let review = chat.review.as_ref()?;
                (review.repo_path == repo_path && review.worktree == worktree).then(|| {
                    let stale = match &heads {
                        Ok((trunk_head, branch_head)) => {
                            *trunk_head != review.trunk_head || *branch_head != review.branch_head
                        }
                        Err(_) => false,
                    };
                    serde_json::json!({
                        "chatId": chat.id,
                        "reviewer": review.reviewer,
                        "trunkHead": review.trunk_head,
                        "branchHead": review.branch_head,
                        "stale": stale,
                        "verdict": review.verdict,
                        "findingCount": review.findings.as_ref().map_or(0, Vec::len),
                        "status": sessions
                            .iter()
                            .find(|session| session.chat_id == chat.id)
                            .map(|session| session.status)
                            .unwrap_or(holt_proto::SessionStatus::Idle),
                    })
                })
            })
            .collect();
        rows.reverse();
        RpcReply::value(&rows)
    }

    /// A registered worktree's path.
    async fn worktree_path(&self, repo_path: &str, worktree: &str) -> Result<String, RpcError> {
        self.git
            .worktree_path(repo_path, worktree)
            .await
            .map_err(RpcError::Failed)
    }

    /// A Space for the review chat to live in: the worktree's own row when
    /// one matches its path (a session worktree's `wt-<chatId>`), else an
    /// ensure-style row for the path — the review chat's cwd must be the
    /// worktree itself.
    fn ensure_review_space(&self, worktree_path: &str) -> Result<holt_proto::Space, String> {
        let device_id = self.engine_info.device_id.clone();
        let git_dir = crate::git::discover_git_dir(std::path::Path::new(worktree_path));
        let checkout_id = git_dir
            .as_ref()
            .map(|git_dir| crate::git::checkout_identity(&device_id, git_dir));
        let mut spaces = self
            .spaces
            .write()
            .map_err(|_| "spaces lock poisoned".to_string())?;
        if let Some(space) = spaces
            .iter()
            .find(|space| space.path.trim_end_matches('/') == worktree_path.trim_end_matches('/'))
        {
            return Ok(space.clone());
        }
        let space = holt_proto::Space {
            id: format!("review-{}", uuid::Uuid::new_v4()),
            device_id,
            path: worktree_path.to_string(),
            name: None,
            git_detected: git_dir.is_some(),
            git_checked_at: Some(chrono::Utc::now()),
            checkout_id,
            created_at: chrono::Utc::now(),
        };
        spaces.push(space.clone());
        crate::store::persist_spaces(&self.data_dir, &spaces).map_err(|error| error.to_string())?;
        let value = serde_json::to_value(&*spaces).map_err(|e| e.to_string())?;
        drop(spaces);
        self.spaces_tx.send_replace(value);
        Ok(space)
    }

    /// The review chat's baseline config: the author chat's when it has
    /// one, else the sticky permission-mode default with no provider —
    /// the first run resolves the default model.
    fn default_review_config(
        &self,
        author_chat: &Option<holt_proto::Chat>,
    ) -> holt_proto::ChatConfig {
        author_chat
            .as_ref()
            .and_then(|chat| chat.config.clone())
            .unwrap_or_else(|| holt_proto::ChatConfig {
                provider: "openai".into(),
                model: String::new(),
                reasoning: None,
                model_options: Default::default(),
                permission_mode: self.mode_default.get(),
                scope: Default::default(),
            })
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewerRow {
    name: String,
    source: String,
}
