//! The engine's git capability on the `git2` backend (ADR-0001).
//!
//! This module is the only place in the workspace that touches git2. Every
//! git2 call runs on the blocking pool — never on the async runtime's thread —
//! and a per-checkout lock, keyed by the repository's common git dir,
//! serializes operations against the same repository while different
//! repositories proceed in parallel.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use git2::{BranchType, Repository};
use holt_proto::{CheckoutDiff, DiffFileSummary, RepoRef, TurnFileChange, TurnFileChangeStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Patch text is capped at 3 MiB; per-file diff sides at 1 MiB each.
pub(crate) const MAX_PATCH_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const MAX_FILE_SIDE_BYTES: usize = 1024 * 1024;

/// Serves the git surface for space folders. One per engine; cheap to clone
/// (the per-checkout locks are shared).
#[derive(Clone, Default)]
pub(crate) struct Git {
    locks: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Git {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Local branches with default-first ordering, `current` tagging, and
    /// linked-worktree paths — the composer branch picker's feed.
    pub(crate) async fn list_refs(&self, repo_path: &str) -> Result<Vec<RepoRef>, String> {
        self.with_repo(repo_path, |repo| Ok(refs_snapshot(&repo)))
            .await
    }

    /// Branch names only, same ordering — feeds the Changes pane's base-ref
    /// defaulting.
    pub(crate) async fn list_branches(&self, repo_path: &str) -> Result<Vec<String>, String> {
        self.with_repo(repo_path, |repo| {
            Ok(refs_snapshot(&repo).into_iter().map(|r| r.name).collect())
        })
        .await
    }

    /// Safe checkout of a local branch — no force, no merge, no stash. A
    /// switch that would overwrite uncommitted data fails with git's own
    /// message; on failure the working tree and HEAD are left untouched.
    pub(crate) async fn switch_ref(
        &self,
        repo_path: &str,
        branch_name: &str,
    ) -> Result<(), String> {
        let branch_name = branch_name.to_string();
        self.with_repo(repo_path, move |repo| switch_branch(&repo, &branch_name))
            .await
    }

    /// Create a branch at `base_ref` (default HEAD) and check it out —
    /// `checkout -b` semantics under the same safe-checkout rules as
    /// [`Git::switch_ref`]. Validation (ref format) and duplicate names are
    /// git's own errors; if the checkout refuses (uncommitted data would be
    /// clobbered) the freshly created branch is rolled back so a retry with
    /// the same name is not met with "already exists".
    pub(crate) async fn create_branch(
        &self,
        repo_path: &str,
        branch_name: &str,
        base_ref: Option<String>,
    ) -> Result<(), String> {
        let branch_name = branch_name.to_string();
        let base_ref = base_ref.filter(|value| !value.trim().is_empty());
        self.with_repo(repo_path, move |repo| {
            create_and_switch(&repo, &branch_name, base_ref.as_deref())
        })
        .await
    }

    /// The working-tree capture for a checkout: uncommitted changes (HEAD →
    /// index → workdir, untracked included), the same content the watch
    /// frame and `GetCheckoutDiff` in working-tree mode serve.
    pub(crate) async fn working_tree(
        &self,
        repo_path: &str,
        device_id: &str,
    ) -> Result<CheckoutDiff, String> {
        let device_id = device_id.to_string();
        self.with_repo(repo_path, move |repo| {
            capture_diff(&repo, &device_id, "workingTree", None).map_err(GitFault::into_string)
        })
        .await
    }

    /// The scoped capture behind `GetCheckoutDiff`: `workingTree` diffs
    /// HEAD → (index → workdir); `branch` diffs `merge-base(baseRef, HEAD)`
    /// → workdir with index, so committed and uncommitted changes on the
    /// branch both appear ("what would this branch ship"). Bad params
    /// (missing/unknown base ref) and hard git failures are told apart by
    /// [`GitFault`].
    pub(crate) async fn capture(
        &self,
        repo_path: &str,
        device_id: &str,
        mode: &str,
        base_ref: Option<&str>,
    ) -> Result<CheckoutDiff, GitFault> {
        let device_id = device_id.to_string();
        let mode = mode.to_string();
        let base_ref = base_ref.map(str::to_string);
        self.with_repo(repo_path, move |repo| {
            capture_diff(&repo, &device_id, &mode, base_ref.as_deref())
        })
        .await
    }

    /// Full old/new text for one file in a capture: old side from the
    /// mode's base tree (HEAD blob in working-tree mode, merge-base blob in
    /// branch mode), new side from the working-tree file. `stale` is set
    /// when the checkout's checksum has moved past the pinned
    /// `diff_checksum` the caller rendered.
    pub(crate) async fn capture_file_text(
        &self,
        repo_path: &str,
        device_id: &str,
        request: &holt_proto::GetCheckoutFileDiffTextRequest,
    ) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
        let device_id = device_id.to_string();
        let request = request.clone();
        self.with_repo(repo_path, move |repo| {
            capture_file_text(&repo, &device_id, &request)
        })
        .await
    }

    /// The per-commit capture behind `GetCheckoutDiff` in commit mode:
    /// parent → commit (root commits against the empty tree); the live
    /// working tree is never read.
    pub(crate) async fn commit_diff(
        &self,
        repo_path: &str,
        device_id: &str,
        commit_sha: &str,
    ) -> Result<CheckoutDiff, GitFault> {
        let device_id = device_id.to_string();
        let commit_sha = commit_sha.to_string();
        self.with_repo(repo_path, move |repo| {
            commit_capture(&repo, &device_id, &commit_sha)
        })
        .await
    }

    /// One page of the topologically ordered commit graph with refs
    /// (branches, remote-tracking, tags) — the History scope's feed.
    pub(crate) async fn history(
        &self,
        repo_path: &str,
        cursor: usize,
        limit: usize,
    ) -> Result<holt_proto::GitHistoryPage, String> {
        self.with_repo(repo_path, move |repo| history_page(&repo, cursor, limit))
            .await
    }

    /// Fetch every remote with prune, using system-default credentials
    /// (credential helpers / ssh-agent — never interactive). Wrapped by the
    /// caller in a 30 s timeout.
    pub(crate) async fn fetch_all(&self, repo_path: &str) -> Result<(), String> {
        self.with_repo::<(), String, _>(repo_path, |repo| {
            let remotes = repo.remotes().map_err(git_message)?;
            for index in 0..remotes.len() {
                let Ok(Some(name)) = remotes.get(index) else {
                    continue;
                };
                let mut remote = repo.find_remote(name).map_err(git_message)?;
                let refspec = format!("refs/heads/*:refs/remotes/{name}/*");
                let mut attempted = false;
                let mut callbacks = git2::RemoteCallbacks::new();
                callbacks.credentials(move |_url, username, allowed| {
                    // One system-default attempt: a credential retry loop
                    // must never become an interactive prompt.
                    if attempted {
                        return Err(git2::Error::from_str("no system credentials available"));
                    }
                    attempted = true;
                    if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
                        git2::Cred::default()
                    } else {
                        git2::Cred::ssh_key_from_agent(username.unwrap_or("git"))
                    }
                });
                let mut options = git2::FetchOptions::new();
                options
                    .prune(git2::FetchPrune::On)
                    .remote_callbacks(callbacks);
                remote
                    .fetch(&[refspec.as_str()], Some(&mut options), Some("holt fetch"))
                    .map_err(|error| format!("{name}: {error}"))?;
            }
            Ok(())
        })
        .await
    }

    /// Stage repo-relative paths into the index (ADR-0022): `git add`
    /// semantics under the per-checkout lock — modifications, untracked
    /// files, recursive directories, and workdir deletions (remove from
    /// index, never a bogus add). The panel's first content-mutating
    /// operation; the agent tool surface stays read-only.
    pub(crate) async fn stage_paths(
        &self,
        repo_path: &str,
        paths: Vec<String>,
    ) -> Result<(), GitFault> {
        self.with_repo(repo_path, move |repo| stage_paths(&repo, &paths))
            .await
    }

    /// Unstage repo-relative paths: reset their index entries to HEAD
    /// (`git reset -- <paths>`), or drop them from the index outright on an
    /// unborn HEAD. A path absent from the index is a successful no-op.
    pub(crate) async fn unstage_paths(
        &self,
        repo_path: &str,
        paths: Vec<String>,
    ) -> Result<(), GitFault> {
        self.with_repo(repo_path, move |repo| unstage_paths(&repo, &paths))
            .await
    }

    /// Commit the staged index and return the new commit's sha. Identity
    /// comes from the repo's effective git config (author == committer);
    /// mid-operation states and a nothing-staged index refuse — see
    /// [`commit_staged`].
    pub(crate) async fn commit_staged(
        &self,
        repo_path: &str,
        message: &str,
    ) -> Result<String, GitFault> {
        let message = message.to_string();
        self.with_repo(repo_path, move |repo| commit_staged(&repo, &message))
            .await
    }

    /// The File sidebar's working-tree status snapshot (file-sidebar ticket
    /// 10): one `statuses()` pass over the checkout — untracked and ignored
    /// directories reported whole, never recursed into — classified into
    /// the concise marker set. Computed fresh against the live working
    /// tree; deliberately NOT one of the checkout-diff family's scoped
    /// captures (working-tree/branch/turn/commit), which key on a chosen
    /// base and must not be inherited as the tree's status.
    pub(crate) async fn workspace_status(&self, repo_path: &str) -> holt_proto::WorkspaceGitStatus {
        // Discover separately from the status pass: a folder outside any
        // work tree is the non-Git Space case (no decorations, no error),
        // while a discovered repository that then fails its pass reports
        // the failure through `error`.
        let discover = tokio::task::spawn_blocking({
            let path = repo_path.to_string();
            move || {
                Repository::discover(&path)
                    .ok()
                    .and_then(|repo| repo.workdir().map(|dir| dir.to_path_buf()))
            }
        })
        .await
        .ok()
        .flatten();
        let Some(workdir) = discover else {
            return holt_proto::WorkspaceGitStatus {
                workdir: None,
                entries: Vec::new(),
                error: None,
            };
        };
        // Canonical spelling: the tree's row paths arrive canonicalized by
        // the listing, so the prefix the UI strips must be too.
        let workdir = workdir.canonicalize().unwrap_or(workdir);
        let display = workdir.display().to_string();
        let workdir_out = display.clone();
        match self
            .with_repo(repo_path, move |repo| {
                let mut options = git2::StatusOptions::new();
                options
                    .include_untracked(true)
                    .recurse_untracked_dirs(false)
                    .include_ignored(true)
                    .recurse_ignored_dirs(false);
                // Rename detection stays off on purpose: libgit2 keys a
                // detected rename at the OLD path, which a live tree cannot
                // show — the marker would vanish. Without detection the new
                // name reports Untracked/Added and stays visible.
                let statuses = repo.statuses(Some(&mut options)).map_err(git_message)?;
                let mut entries = Vec::new();
                for position in 0..statuses.len() {
                    let Some(entry) = statuses.get(position) else {
                        continue;
                    };
                    let Ok(path) = entry.path() else { continue };
                    let Some(kind) = classify_status(entry.status()) else {
                        continue;
                    };
                    let (index_kind, worktree_kind) = status_sides(entry.status());
                    entries.push(holt_proto::WorkspaceGitStatusEntry {
                        // libgit2 spells wholly-untracked/ignored
                        // directories with a trailing `/`; strip it so one
                        // key matches the directory row and, through the
                        // UI's ancestor walk, its descendants. The raw
                        // spelling's trailing `/` survives as `is_dir`.
                        is_dir: path.ends_with('/'),
                        path: path.trim_end_matches('/').to_string(),
                        kind,
                        index: index_kind,
                        worktree: worktree_kind,
                    });
                }
                entries.sort_by(|a, b| a.path.cmp(&b.path));
                entries.dedup_by(|a, b| a.path == b.path);
                Ok::<_, String>(holt_proto::WorkspaceGitStatus {
                    workdir: Some(display),
                    entries,
                    error: None,
                })
            })
            .await
        {
            Ok(snapshot) => snapshot,
            Err(message) => holt_proto::WorkspaceGitStatus {
                workdir: Some(workdir_out),
                entries: Vec::new(),
                error: Some(message),
            },
        }
    }

    /// Capture a turn baseline (ADR-0003): `(HEAD sha, uncommitted patch)`
    /// for a checkout, the patch under the shared 3 MiB cap.
    pub(crate) async fn turn_baseline(&self, repo_path: &str) -> Result<TurnBaseline, String> {
        self.with_repo(repo_path, |repo| {
            let head_sha = repo
                .head()
                .ok()
                .and_then(|head| head.peel_to_commit().ok())
                .map(|commit| commit.id().to_string())
                .unwrap_or_else(|| EMPTY_HEAD.to_string());
            let head_tree = repo
                .head()
                .ok()
                .and_then(|head| head.peel_to_commit().ok())
                .and_then(|commit| commit.tree().ok());
            let mut options = git2::DiffOptions::new();
            options
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true)
                .show_binary(true);
            let mut diff = repo
                .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))
                .map_err(git_message)?;
            let mut find = git2::DiffFindOptions::new();
            find.renames(true).for_untracked(true);
            let _ = diff.find_similar(Some(&mut find));
            let (patch, _truncated) = truncate_patch(&diff_patch_text(&mut diff), MAX_PATCH_BYTES);
            Ok(TurnBaseline { head_sha, patch })
        })
        .await
    }

    /// The Turn identity stamp (ADR-0007): the working directory's live HEAD
    /// — branch, head sha, repo root, and the canonical checkout identity —
    /// captured at Run acceptance. `None` when the folder is not a git work
    /// tree or HEAD carries no branch: nothing is stamped, nothing fails.
    pub(crate) async fn turn_source_context(
        &self,
        repo_path: &str,
        device_id: &str,
    ) -> Option<holt_proto::ConversationSourceContext> {
        let device_id = device_id.to_string();
        let cwd = repo_path.to_string();
        self.with_repo(repo_path, move |repo| {
            Ok::<_, String>(source_context_stamp(&repo, &device_id, &cwd))
        })
        .await
        .ok()
        .flatten()
    }

    /// The "Latest turn" capture: `HEAD@start → workdir`, net-change
    /// filtered against the turn baseline. Available live while a run is
    /// in flight; re-keys on the CURRENT head so commits during the turn
    /// refetch.
    pub(crate) async fn turn_diff(
        &self,
        repo_path: &str,
        device_id: &str,
        baseline: &TurnBaseline,
    ) -> Result<CheckoutDiff, GitFault> {
        let device_id = device_id.to_string();
        let baseline = baseline.clone();
        self.with_repo(repo_path, move |repo| {
            turn_capture(&repo, &device_id, &baseline)
        })
        .await
    }

    /// Per-file text for the turn scope; see [`turn_file_text_blocking`].
    /// `stale` is computed against a fresh turn recompute.
    pub(crate) async fn turn_file_text(
        &self,
        repo_path: &str,
        device_id: &str,
        request: &holt_proto::GetCheckoutFileDiffTextRequest,
        baseline: &TurnBaseline,
    ) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
        let device_id = device_id.to_string();
        let request = request.clone();
        let baseline = baseline.clone();
        self.with_repo(repo_path, move |repo| {
            let mut text = turn_file_text_blocking(&repo, &request, &baseline)?;
            let current = turn_capture(&repo, &device_id, &baseline)?;
            text.stale = request.diff_checksum != current.checksum;
            Ok(text)
        })
        .await
    }

    /// The Turn change set (ADR-0024): the net change from a Turn's
    /// baseline to the live working tree, as the typed vocabulary the Turn
    /// card and its Review render. Reuses the turn capture's Git path —
    /// dirty-start net-change filtering, rename detection, and the patch
    /// cap — and re-keys nothing: the caller adds Turn identity and phase.
    pub(crate) async fn turn_change_capture(
        &self,
        repo_path: &str,
        device_id: &str,
        baseline: &TurnBaseline,
    ) -> Result<TurnChangeCapture, GitFault> {
        let diff = self.turn_diff(repo_path, device_id, baseline).await?;
        let mut files: Vec<TurnFileChange> = diff.files.iter().map(turn_file_change).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(TurnChangeCapture {
            files,
            additions: diff.additions,
            deletions: diff.deletions,
            truncated: diff.truncated,
        })
    }

    /// The immutable per-file before/after content of a frozen change set
    /// (ADR-0024 ticket 02), captured once at Turn settlement: the old side
    /// is each file's turn-start content — read at its turn-start path for a
    /// rename — and the new side the working-tree file at settle time. This
    /// is the pair that persists, so later workspace edits and restarts
    /// cannot move a settled Turn's history. Captured together with the
    /// summary in ONE locked Git pass, so the two can never disagree about
    /// what the tree held at settlement.
    pub(crate) async fn turn_change_freeze(
        &self,
        repo_path: &str,
        device_id: &str,
        baseline: &TurnBaseline,
    ) -> Result<TurnChangeFreeze, GitFault> {
        let device_id = device_id.to_string();
        let baseline = baseline.clone();
        self.with_repo(repo_path, move |repo| {
            let diff = turn_capture(&repo, &device_id, &baseline)?;
            let mut files: Vec<TurnFileChange> = diff.files.iter().map(turn_file_change).collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let content = files
                .iter()
                .map(|file| {
                    let old = turn_start_side(
                        &repo,
                        file.old_path.as_deref().unwrap_or(&file.path),
                        &baseline,
                    )?;
                    let new = workdir_side(&repo, &file.path);
                    Ok(TurnFileContent {
                        path: file.path.clone(),
                        old_text: old.text,
                        old_content_hash: old.content_hash,
                        new_text: new.text,
                        new_content_hash: new.content_hash,
                        binary: old.binary || new.binary,
                        truncated: old.truncated || new.truncated,
                    })
                })
                .collect::<Result<Vec<_>, GitFault>>()?;
            Ok(TurnChangeFreeze {
                capture: TurnChangeCapture {
                    files,
                    additions: diff.additions,
                    deletions: diff.deletions,
                    truncated: diff.truncated,
                },
                content,
            })
        })
        .await
    }

    /// Whether `repo_path` resolves to a Git work tree. The change set's
    /// non-Git answer keys on this, never on a capture error.
    pub(crate) async fn is_work_tree(&self, repo_path: &str) -> bool {
        let path = repo_path.to_string();
        tokio::task::spawn_blocking(move || Repository::discover(&path).is_ok())
            .await
            .unwrap_or(false)
    }

    /// Run `op` against the repository resolved from `repo_path`: resolve
    /// its common git dir, take the per-checkout lock, then execute on the
    /// blocking pool.
    async fn with_repo<T, E, F>(&self, repo_path: &str, op: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<String> + Send + 'static,
        F: FnOnce(Repository) -> Result<T, E> + Send + 'static,
    {
        let repo_path = repo_path.to_string();
        let discover_path = repo_path.clone();
        let lock_key = tokio::task::spawn_blocking(move || {
            let repo = Repository::discover(&discover_path).map_err(git_message)?;
            Ok::<_, String>(normalize(repo.commondir()))
        })
        .await
        .map_err(|error| E::from(error.to_string()))??;
        let guard = self.lock_for(lock_key).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let repo = Repository::discover(&repo_path).map_err(git_message)?;
            op(repo)
        })
        .await
        .map_err(|error| E::from(error.to_string()))?
    }

    fn lock_for(&self, key: PathBuf) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|error| error.into_inner());
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// Default-branch precedence: the branch `origin/HEAD` targets when that
/// branch exists locally, else `main`, else `master`, else the first name
/// alphabetically. `None` only for an empty branch list.
pub(crate) fn default_branch<'a>(
    names: &'a [String],
    origin_head_target: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(target) = origin_head_target
        && names.iter().any(|name| name == target)
    {
        return Some(target);
    }
    for candidate in ["main", "master"] {
        if let Some(found) = names.iter().find(|name| name.as_str() == candidate) {
            return Some(found);
        }
    }
    names.iter().map(String::as_str).min()
}

/// The picker's ref list: local branches only, default first, the rest
/// alphabetical, `current` on the checked-out branch, `worktreePath` on
/// branches checked out in linked worktrees.
fn refs_snapshot(repo: &Repository) -> Vec<RepoRef> {
    let mut names = match local_branch_names(repo) {
        Ok(names) => names,
        Err(_) => return Vec::new(),
    };
    if names.is_empty() {
        return Vec::new();
    }
    let default = default_branch(&names, origin_head_target(repo).as_deref()).map(str::to_string);
    names.sort();
    let current = current_branch(repo);
    let worktree_heads = linked_worktree_heads(repo);
    let mut ordered: Vec<String> = Vec::with_capacity(names.len());
    if let Some(default) = default.as_deref() {
        ordered.push(default.to_string());
    }
    ordered.extend(
        names
            .into_iter()
            .filter(|name| Some(name.as_str()) != default.as_deref()),
    );
    ordered
        .into_iter()
        .map(|name| RepoRef {
            worktree_path: worktree_heads
                .iter()
                .find(|(head, _)| *head == name)
                .map(|(_, path)| path.display().to_string()),
            current: Some(name.clone()) == current,
            name,
        })
        .collect()
}

fn local_branch_names(repo: &Repository) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for branch in repo
        .branches(Some(BranchType::Local))
        .map_err(git_message)?
        .filter_map(Result::ok)
    {
        let (branch, _) = branch;
        if let Some(name) = branch.name().map_err(git_message)? {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// The branch name `refs/remotes/origin/HEAD` points at, if it is a healthy
/// symbolic ref into origin's namespace.
fn origin_head_target(repo: &Repository) -> Option<String> {
    let head = repo.find_reference("refs/remotes/origin/HEAD").ok()?;
    head.symbolic_target()
        .ok()??
        .strip_prefix("refs/remotes/origin/")
        .map(str::to_string)
}

/// `(branch, worktree path)` for every linked worktree with a branch (not a
/// detached HEAD) checked out.
fn linked_worktree_heads(repo: &Repository) -> Vec<(String, PathBuf)> {
    let mut heads = Vec::new();
    let Ok(names) = repo.worktrees() else {
        return heads;
    };
    for index in 0..names.len() {
        let Ok(Some(name)) = names.get(index) else {
            continue;
        };
        let Ok(worktree) = repo.find_worktree(name) else {
            continue;
        };
        let path = worktree.path().to_path_buf();
        let Ok(worktree_repo) = Repository::open(&path) else {
            continue;
        };
        let Ok(head) = worktree_repo.head() else {
            continue;
        };
        if !head.is_branch() {
            continue;
        }
        let Ok(shorthand) = head.shorthand() else {
            continue;
        };
        heads.push((shorthand.to_string(), path));
    }
    heads
}

fn current_branch(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().ok().map(str::to_string)
}

/// Build the [`holt_proto::ConversationSourceContext`] for a working
/// directory's live HEAD: `None` when HEAD carries no branch (detached) or
/// the repository has no work tree. The checkout id hashes the checkout's
/// OWN git dir, so a linked worktree never collides with its parent (see
/// [`checkout_identity`]).
fn source_context_stamp(
    repo: &Repository,
    device_id: &str,
    cwd: &str,
) -> Option<holt_proto::ConversationSourceContext> {
    let branch = current_branch(repo)?;
    let head_sha = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id().to_string());
    Some(holt_proto::ConversationSourceContext {
        checkout_id: checkout_identity(device_id, &normalize(repo.path())),
        repo_root: repo.workdir()?.display().to_string(),
        cwd: cwd.to_string(),
        branch,
        head_sha,
        observed_at: chrono::Utc::now(),
    })
}

/// Create nothing, merge nothing, stash nothing: resolve the branch, check
/// its tree out with git's SAFE strategy, then move HEAD. A checkout that
/// would clobber uncommitted data fails before HEAD moves — with the
/// blocking file paths in [`switch_refusal_message`] — and the safe
/// checkout behind it stays as the race-day guard, with git's message
/// verbatim.
fn switch_branch(repo: &Repository, branch_name: &str) -> Result<(), String> {
    let branch = repo
        .find_branch(branch_name, BranchType::Local)
        .map_err(git_message)?;
    let reference = branch.into_reference();
    let ref_name = reference.name().map_err(git_message)?.to_string();
    let treeish = reference
        .peel(git2::ObjectType::Tree)
        .map_err(git_message)?;
    let tree = treeish
        .as_tree()
        .ok_or_else(|| "ref does not point at a tree".to_string())?;
    let conflicts = switch_conflicts(repo, tree)?;
    if !conflicts.is_empty() {
        return Err(switch_refusal_message(&conflicts));
    }
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(&treeish, Some(&mut checkout))
        .map_err(git_message)?;
    repo.set_head(&ref_name).map_err(git_message)?;
    Ok(())
}

/// The files git's safe checkout would refuse to overwrite: paths with
/// uncommitted changes (index or worktree, untracked included) that the
/// target tree also changes relative to HEAD. Sorted for a stable message.
fn switch_conflicts(repo: &Repository, target_tree: &git2::Tree) -> Result<Vec<String>, String> {
    let head_tree = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .and_then(|commit| commit.tree().ok());
    let mut options = git2::DiffOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let dirty = repo
        .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))
        .map_err(git_message)?;
    let dirty_paths: Vec<String> = dirty.deltas().filter_map(delta_path).collect();
    let changed = repo
        .diff_tree_to_tree(head_tree.as_ref(), Some(target_tree), None)
        .map_err(git_message)?;
    let changed_paths: std::collections::HashSet<String> =
        changed.deltas().filter_map(delta_path).collect();
    let mut conflicts: Vec<String> = dirty_paths
        .into_iter()
        .filter(|path| changed_paths.contains(path))
        .collect();
    conflicts.sort();
    conflicts.dedup();
    Ok(conflicts)
}

/// A delta's path, new side first (renames read as their post-move name).
fn delta_path(delta: git2::DiffDelta) -> Option<String> {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .map(|path| path.display().to_string())
}

/// The refusal the UI's switch dialog parses (ADR-0007 — inform-only, never
/// a force/stash escape hatch): the shared [`SWITCH_REFUSAL_MARKER`] line,
/// then one blocking path per line.
pub(crate) fn switch_refusal_message(files: &[String]) -> String {
    let mut message = String::from(holt_proto::SWITCH_REFUSAL_MARKER);
    for file in files {
        message.push('\n');
        message.push_str(file);
    }
    message
}

/// `checkout -b`: git itself validates the name (ref-format rules) and
/// rejects duplicates when the branch is created; only then does the safe
/// checkout run. A refused checkout rolls the fresh branch back so a retry
/// with the same name starts clean.
fn create_and_switch(
    repo: &Repository,
    branch_name: &str,
    base_ref: Option<&str>,
) -> Result<(), String> {
    let commit = match base_ref {
        Some(base) => repo
            .revparse_single(base)
            .map_err(git_message)?
            .peel_to_commit()
            .map_err(git_message)?,
        None => repo
            .head()
            .map_err(git_message)?
            .peel_to_commit()
            .map_err(git_message)?,
    };
    let mut branch = repo
        .branch(branch_name, &commit, false)
        .map_err(git_message)?;
    let branch_name = branch_name.to_string();
    match switch_branch(repo, &branch_name) {
        Ok(()) => Ok(()),
        Err(message) => {
            branch.delete().ok();
            Err(message)
        }
    }
}

// ---- staging and commit (ADR-0022) ----

/// The write trio's path contract: at least one path, each repo-relative
/// with no escapes — absolute paths and `..` components are bad params.
fn validate_repo_paths(paths: &[String]) -> Result<(), GitFault> {
    if paths.is_empty() {
        return Err(GitFault::BadParams(
            "paths must list at least one repo-relative path".into(),
        ));
    }
    for path in paths {
        let parsed = Path::new(path);
        if path.trim().is_empty() {
            return Err(GitFault::BadParams("paths must not be empty".into()));
        }
        if parsed.is_absolute() {
            return Err(GitFault::BadParams(format!(
                "paths must be repo-relative, not absolute: {path}"
            )));
        }
        if parsed
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(GitFault::BadParams(format!(
                "paths must not escape the repository: {path}"
            )));
        }
    }
    Ok(())
}

/// Refuse to stage or unstage any requested path that IS — or contains — a
/// conflicted index entry: libgit2 would silently mark the conflict
/// resolved, discarding the merge information the user still needs.
fn refuse_conflicted(repo: &Repository, paths: &[String]) -> Result<(), GitFault> {
    let index = repo.index().map_err(git_fail)?;
    if !index.has_conflicts() {
        return Ok(());
    }
    let mut conflicted: Vec<String> = Vec::new();
    let conflicts = index.conflicts().map_err(git_fail)?;
    for conflict in conflicts {
        let conflict = conflict.map_err(git_fail)?;
        for side in [conflict.ancestor, conflict.our, conflict.their]
            .into_iter()
            .flatten()
        {
            conflicted.push(String::from_utf8_lossy(&side.path).into_owned());
        }
    }
    for requested in paths {
        let hit = conflicted
            .iter()
            .any(|path| path == requested || path.starts_with(&format!("{requested}/")));
        if hit {
            return Err(GitFault::Error(format!(
                "{requested} has unresolved merge conflicts — resolve them first"
            )));
        }
    }
    Ok(())
}

/// `git add <paths>`: one `add_all` pass stages modifications, untracked
/// files (directories recursively), and deletions of paths the working
/// tree no longer has — the deletion lands as a remove-from-index, never a
/// bogus add of a missing file.
fn stage_paths(repo: &Repository, paths: &[String]) -> Result<(), GitFault> {
    validate_repo_paths(paths)?;
    refuse_conflicted(repo, paths)?;
    let mut index = repo.index().map_err(git_fail)?;
    index
        .add_all(paths.iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(git_fail)?;
    index.write().map_err(git_fail)?;
    Ok(())
}

/// `git reset -- <paths>`: reset each path's index entry to HEAD. With an
/// unborn HEAD there is nothing to reset against, so the paths drop out of
/// the index outright (`git rm --cached` semantics); either way a path
/// absent from the index is a successful no-op.
fn unstage_paths(repo: &Repository, paths: &[String]) -> Result<(), GitFault> {
    validate_repo_paths(paths)?;
    refuse_conflicted(repo, paths)?;
    match repo.head() {
        Ok(head) => {
            let target = head.peel(git2::ObjectType::Any).map_err(git_fail)?;
            repo.reset_default(Some(&target), paths.iter())
                .map_err(git_fail)?;
        }
        Err(error) if unborn(&error) => {
            let mut index = repo.index().map_err(git_fail)?;
            let wanted: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let mut to_remove: Vec<PathBuf> = Vec::new();
            for position in 0..index.len() {
                let Some(entry) = index.get(position) else {
                    continue;
                };
                let entry_path = PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned());
                if wanted
                    .iter()
                    .any(|path| entry_path == *path || entry_path.starts_with(path))
                {
                    to_remove.push(entry_path);
                }
            }
            for path in to_remove {
                match index.remove_path(&path) {
                    Ok(()) => {}
                    // Absent from the index: the no-op case.
                    Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                    Err(error) => return Err(GitFault::Error(git_message(error))),
                }
            }
            index.write().map_err(git_fail)?;
        }
        Err(error) => return Err(GitFault::Error(git_message(error))),
    }
    Ok(())
}

/// `repo.head()` reports an unborn branch (no commits yet) as UnbornBranch
/// or NotFound depending on how HEAD is set — both mean "no HEAD commit".
fn unborn(error: &git2::Error) -> bool {
    matches!(
        error.code(),
        git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
    )
}

/// Commit the staged index on HEAD, returning the new sha. Safety gates,
/// in order: a blank message is bad params; any in-progress operation
/// (merge, rebase, revert, cherry-pick — even with all conflicts resolved,
/// where the merge state persists) refuses, because git2's `commit()` does
/// not pick up `MERGE_HEAD` and would silently drop the merge parentage;
/// and a nothing-staged index refuses like `git commit` does. Identity is
/// the repo's effective `user.name` / `user.email`, author == committer —
/// a missing identity fails with an actionable message. On a detached HEAD
/// the commit advances HEAD directly and moves no branch; on an unborn
/// HEAD it creates the parentless root commit.
fn commit_staged(repo: &Repository, message: &str) -> Result<String, GitFault> {
    if message.trim().is_empty() {
        return Err(GitFault::BadParams(
            "commit message must not be blank".into(),
        ));
    }
    let state = repo.state();
    if state != git2::RepositoryState::Clean {
        return Err(GitFault::Error(format!(
            "cannot commit while a {} is in progress — finish or abort it first",
            operation_label(state),
        )));
    }
    let head_commit = match repo.head() {
        Ok(head) => Some(head.peel_to_commit().map_err(git_fail)?),
        Err(error) if unborn(&error) => None,
        Err(error) => return Err(GitFault::Error(git_message(error))),
    };
    let mut index = repo.index().map_err(git_fail)?;
    let staged = match &head_commit {
        Some(commit) => {
            let head_tree = commit.tree().map_err(git_fail)?;
            let diff = repo
                .diff_tree_to_index(Some(&head_tree), Some(&index), None)
                .map_err(git_fail)?;
            diff.deltas().len() > 0
        }
        None => !index.is_empty(),
    };
    if !staged {
        return Err(GitFault::Error("nothing staged to commit".into()));
    }
    let signature = commit_identity(repo)?;
    let tree_id = index.write_tree().map_err(git_fail)?;
    let tree = repo.find_tree(tree_id).map_err(git_fail)?;
    let parents: Vec<&git2::Commit> = head_commit.iter().collect();
    let oid = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .map_err(git_fail)?;
    Ok(oid.to_string())
}

/// The commit signature: `user.name` / `user.email` from the repo's
/// effective config (local → global → system), author == committer,
/// exactly like `git commit`. Missing or blank values fail with the
/// configuration hint the UI shows verbatim.
fn commit_identity(repo: &Repository) -> Result<git2::Signature<'static>, GitFault> {
    let config = repo.config().map_err(git_fail)?;
    let name = config
        .get_string("user.name")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let email = config
        .get_string("user.email")
        .ok()
        .filter(|value| !value.trim().is_empty());
    match (name, email) {
        (Some(name), Some(email)) => git2::Signature::now(&name, &email).map_err(git_fail),
        _ => Err(GitFault::Error(
            "no git identity configured — set user.name and user.email, e.g. \
             `git config user.name \"Your Name\"` and `git config user.email you@example.com`"
                .into(),
        )),
    }
}

/// The in-progress operation a non-Clean `RepositoryState` represents, for
/// the commit refusal message.
fn operation_label(state: git2::RepositoryState) -> &'static str {
    use git2::RepositoryState as State;
    match state {
        State::Merge => "merge",
        State::Revert | State::RevertSequence => "revert",
        State::CherryPick | State::CherryPickSequence => "cherry-pick",
        State::Rebase
        | State::RebaseInteractive
        | State::RebaseMerge
        | State::ApplyMailboxOrRebase => "rebase",
        State::ApplyMailbox => "am",
        State::Bisect => "bisect",
        _ => "operation",
    }
}

fn git_message(error: git2::Error) -> String {
    error.message().to_string()
}

/// One status entry's concise classification for the File sidebar's
/// markers (ticket 10). `None` for entries that carry no working-tree
/// signal (clean files). Conflicted paths report `Conflicted` (ADR-0022):
/// both the sidebar and the Git panel need honest conflict state without
/// deriving it, so the collapsed kind no longer folds it into `Modified`.
fn classify_status(status: git2::Status) -> Option<holt_proto::WorkspaceGitStatusKind> {
    use holt_proto::WorkspaceGitStatusKind as Kind;
    if status.contains(git2::Status::IGNORED) {
        Some(Kind::Ignored)
    } else if status.contains(git2::Status::CONFLICTED) {
        Some(Kind::Conflicted)
    } else if status.contains(git2::Status::INDEX_NEW) {
        Some(Kind::Added)
    } else if status.contains(git2::Status::WT_NEW) {
        Some(Kind::Untracked)
    } else if status.intersects(git2::Status::INDEX_DELETED | git2::Status::WT_DELETED) {
        Some(Kind::Deleted)
    } else if status.intersects(
        git2::Status::INDEX_MODIFIED
            | git2::Status::INDEX_RENAMED
            | git2::Status::INDEX_TYPECHANGE
            | git2::Status::WT_MODIFIED
            | git2::Status::WT_RENAMED
            | git2::Status::WT_TYPECHANGE,
    ) {
        Some(Kind::Modified)
    } else {
        None
    }
}

/// The porcelain sides of one status entry (ADR-0022): the index side is
/// what staging captured (`git status --porcelain`'s X column), the
/// worktree side the unstaged state (the Y column, untracked included).
/// Each side collapses its own bits independently, so a file modified
/// before AND after staging reports both halves. A conflicted path reports
/// `Conflicted` on BOTH sides — porcelain never shows blank columns for an
/// unmerged path (UU/AA/DD fill both), and "(clean, clean)" would contradict
/// the entry's collapsed kind. Ignored entries carry no sides — the
/// collapsed kind says all there is to say.
fn status_sides(
    status: git2::Status,
) -> (
    Option<holt_proto::WorkspaceGitStatusKind>,
    Option<holt_proto::WorkspaceGitStatusKind>,
) {
    use holt_proto::WorkspaceGitStatusKind as Kind;
    if status.contains(git2::Status::CONFLICTED) {
        return (Some(Kind::Conflicted), Some(Kind::Conflicted));
    }
    let index = if status.contains(git2::Status::INDEX_NEW) {
        Some(Kind::Added)
    } else if status.contains(git2::Status::INDEX_DELETED) {
        Some(Kind::Deleted)
    } else if status.intersects(
        git2::Status::INDEX_MODIFIED | git2::Status::INDEX_RENAMED | git2::Status::INDEX_TYPECHANGE,
    ) {
        Some(Kind::Modified)
    } else {
        None
    };
    let worktree = if status.contains(git2::Status::WT_NEW) {
        Some(Kind::Untracked)
    } else if status.contains(git2::Status::WT_DELETED) {
        Some(Kind::Deleted)
    } else if status.intersects(
        git2::Status::WT_MODIFIED | git2::Status::WT_RENAMED | git2::Status::WT_TYPECHANGE,
    ) {
        Some(Kind::Modified)
    } else {
        None
    };
    (index, worktree)
}

/// Collapse a git dir to its component form so `.git/` and `.git` hash as
/// the same lock key.
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

// ---- checkout identity ----

/// Canonical checkout identity (ADR-0002): `sha256(deviceId ‖ NUL ‖ git_dir)`,
/// hex-encoded. `git_dir` is the checkout's OWN git dir — a linked worktree's
/// `.git/worktrees/<name>`, not the common dir — so two worktrees of one
/// repository never collide.
pub(crate) fn checkout_identity(device_id: &str, git_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_id.as_bytes());
    hasher.update([0]);
    hasher.update(git_dir.display().to_string().as_bytes());
    hex(&hasher.finalize())
}

/// Discover a folder's own git dir, if the folder sits inside a work tree.
pub(crate) fn discover_git_dir(path: &Path) -> Option<PathBuf> {
    Repository::discover(path)
        .ok()
        .map(|repo| normalize(repo.path()))
}

// ---- diff capture ----

/// The diff checksum formula (mirrored on the proto field's doc):
/// `sha256(head_sha ‖ NUL ‖ mode ‖ baseRef ‖ patch_bytes)`, hex-encoded.
/// `base_ref` is the empty string when the scope has none. HEAD and the
/// scope fold in, so a commit that leaves the patch text identical still
/// re-keys the capture.
pub(crate) fn diff_checksum(head_sha: &str, mode: &str, base_ref: &str, patch: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(head_sha.as_bytes());
    hasher.update([0]);
    hasher.update(mode.as_bytes());
    hasher.update(base_ref.as_bytes());
    hasher.update(patch.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Cap patch text at `cap` bytes, cutting only BETWEEN hunks — never inside
/// one — so every emitted file section stays parseable. File headers and
/// whole hunks accumulate while they fit; the first segment is always kept
/// so truncation always makes progress. Returns the (possibly shortened)
/// patch and whether anything was dropped.
pub(crate) fn truncate_patch(patch: &str, cap: usize) -> (String, bool) {
    if patch.len() <= cap {
        return (patch.to_string(), false);
    }
    // Segment boundaries: each "diff --git" header and each "@@" hunk
    // header starts a new segment, so a cut never lands inside a hunk.
    let lines: Vec<&str> = patch.lines().collect();
    let mut segments: Vec<(usize, usize)> = Vec::new(); // [start, end) line ranges
    let mut start = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let boundary = line.starts_with("diff --git ") || line.starts_with("@@");
        if boundary && index > start {
            segments.push((start, index));
            start = index;
        }
    }
    segments.push((start, lines.len()));

    let mut kept_end = 0usize; // line index (exclusive) kept so far
    let mut kept_bytes = 0usize;
    let mut whole_hunks = 0usize;
    for (position, (seg_start, seg_end)) in segments.iter().enumerate() {
        let mut segment_bytes = 0usize;
        for line in &lines[*seg_start..*seg_end] {
            segment_bytes += line.len() + 1; // + newline
        }
        if position > 0 && kept_bytes + segment_bytes > cap {
            break;
        }
        if position > 0 {
            whole_hunks += 1;
        }
        kept_end = *seg_end;
        kept_bytes += segment_bytes;
    }
    // Pathological case: not even one whole hunk fits (a single hunk larger
    // than the whole cap). Cut that hunk at a line boundary so the byte cap
    // still holds and the patch keeps real content.
    if whole_hunks == 0 && kept_end < lines.len() {
        while kept_end < lines.len() {
            let add = lines[kept_end].len() + 1;
            if kept_bytes + add > cap {
                break;
            }
            kept_bytes += add;
            kept_end += 1;
        }
    }
    let mut out = String::new();
    for line in &lines[..kept_end] {
        out.push_str(line);
        out.push('\n');
    }
    (out, kept_end < lines.len())
}

/// The null oid as git prints it — the HEAD sha stand-in for a repo with no
/// commits.
const EMPTY_HEAD: &str = "0000000000000000000000000000000000000000";

/// A git capture failure, split by how the RPC layer should report it:
/// caller-input problems (`BadParams`) versus repository failures.
#[derive(Debug)]
pub(crate) enum GitFault {
    BadParams(String),
    Error(String),
}

impl GitFault {
    fn into_string(self) -> String {
        match self {
            Self::BadParams(message) | Self::Error(message) => message,
        }
    }
}

impl From<String> for GitFault {
    fn from(message: String) -> Self {
        Self::Error(message)
    }
}

/// Wrap a git2 failure as a repository-level [`GitFault::Error`].
fn git_fail(error: git2::Error) -> GitFault {
    GitFault::Error(git_message(error))
}

/// Resolve the diff base tree for a capture mode. `workingTree` keys on
/// HEAD; `branch` keys on `merge-base(baseRef, HEAD)` so committed and
/// uncommitted work on the branch both appear.
fn base_tree_for<'repo>(
    repo: &'repo Repository,
    mode: &str,
    base_ref: Option<&str>,
    head_tree: Option<&git2::Tree<'repo>>,
) -> Result<Option<git2::Tree<'repo>>, GitFault> {
    match mode {
        "workingTree" => Ok(head_tree.cloned()),
        "branch" => {
            let base_ref = base_ref
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    GitFault::BadParams("baseRef is required for branch diffs".into())
                })?;
            let base_commit = repo
                .revparse_single(base_ref)
                .map_err(|_| GitFault::BadParams(format!("unknown base ref: {base_ref}")))?
                .peel_to_commit()
                .map_err(|_| {
                    GitFault::BadParams(format!("base ref is not a commit: {base_ref}"))
                })?;
            let head_commit = repo
                .head()
                .map_err(|_| GitFault::Error("this repository has no commits".into()))?
                .peel_to_commit()
                .map_err(git_message)
                .map_err(GitFault::Error)?;
            let merge_base = repo
                .merge_base(base_commit.id(), head_commit.id())
                .map_err(|_| {
                    GitFault::Error(format!("no common ancestry between {base_ref} and HEAD"))
                })?;
            let tree = repo
                .find_commit(merge_base)
                .and_then(|commit| commit.tree())
                .map_err(git_message)
                .map_err(GitFault::Error)?;
            Ok(Some(tree))
        }
        other => Err(GitFault::BadParams(format!(
            "unsupported diff mode: {other}"
        ))),
    }
}

/// Capture a checkout's diff (base tree → index → workdir, untracked
/// included, rename-detected) as a wire `CheckoutDiff`. The mode and base
/// ref fold into the checksum per the documented formula.
fn capture_diff(
    repo: &Repository,
    device_id: &str,
    mode: &str,
    base_ref: Option<&str>,
) -> Result<CheckoutDiff, GitFault> {
    let head = repo.head().ok().and_then(|head| {
        head.peel_to_commit()
            .ok()
            .map(|commit| (commit.id().to_string(), commit.tree().ok()))
    });
    let (head_sha, head_tree) = match head {
        Some((sha, tree)) => (sha, tree),
        None => (EMPTY_HEAD.to_string(), None),
    };
    let base_tree = base_tree_for(repo, mode, base_ref, head_tree.as_ref())?;

    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        // Untracked deltas carry no content unless asked: the working-tree
        // capture must show new files' additions like `git diff` would.
        .show_untracked_content(true)
        .show_binary(true);
    let mut diff = repo
        .diff_tree_to_workdir_with_index(base_tree.as_ref(), Some(&mut options))
        .map_err(git_message)
        .map_err(GitFault::Error)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).for_untracked(true);
    // Rename detection is best-effort: a failure leaves the raw diff intact.
    let _ = diff.find_similar(Some(&mut find));

    Ok(diff_to_payload(
        repo,
        &mut diff,
        device_id,
        mode,
        base_ref.unwrap_or(""),
        &head_sha,
    ))
}

/// The per-commit capture behind `GetCheckoutDiff` in commit mode:
/// parent tree → commit tree, never touching the working tree. A root
/// commit diffs against the empty tree.
pub(crate) fn commit_capture(
    repo: &Repository,
    device_id: &str,
    commit_sha: &str,
) -> Result<CheckoutDiff, GitFault> {
    let commit = repo
        .revparse_single(commit_sha)
        .map_err(|_| GitFault::BadParams(format!("unknown commit: {commit_sha}")))?
        .peel_to_commit()
        .map_err(|_| GitFault::BadParams(format!("not a commit: {commit_sha}")))?;
    let commit_tree = commit
        .tree()
        .map_err(git_message)
        .map_err(GitFault::Error)?;
    let parent_tree = match commit.parent_count() {
        0 => None,
        _ => Some(
            commit
                .parent(0)
                .map_err(git_message)
                .map_err(GitFault::Error)?
                .tree()
                .map_err(git_message)
                .map_err(GitFault::Error)?,
        ),
    };
    let mut options = git2::DiffOptions::new();
    options.show_binary(true);
    let mut diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(&mut options))
        .map_err(git_message)
        .map_err(GitFault::Error)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    let _ = diff.find_similar(Some(&mut find));
    Ok(diff_to_payload(
        repo,
        &mut diff,
        device_id,
        "commit",
        "",
        &commit.id().to_string(),
    ))
}

/// Turn a prepared diff into the wire `CheckoutDiff`: summaries, counts,
/// capped patch, checksum. Shared by every capture mode.
fn diff_to_payload(
    repo: &Repository,
    diff: &mut git2::Diff,
    device_id: &str,
    mode: &str,
    base_ref: &str,
    head_sha: &str,
) -> CheckoutDiff {
    let files = diff_summaries(diff);
    let patch = diff_patch_text(diff);
    let (patch, truncated) = truncate_patch(&patch, MAX_PATCH_BYTES);

    let additions: u32 = files.iter().map(|file| file.additions).sum();
    let deletions: u32 = files.iter().map(|file| file.deletions).sum();
    let workdir = repo
        .workdir()
        .map(|path| path.display().to_string().trim_end_matches('/').to_string())
        .unwrap_or_default();
    CheckoutDiff {
        checksum: diff_checksum(head_sha, mode, base_ref, &patch),
        checkout_id: checkout_identity(device_id, repo.path()),
        device_id: device_id.to_string(),
        cwd: workdir,
        additions,
        deletions,
        patch,
        files,
        truncated,
        updated_at: chrono::Utc::now(),
    }
}

/// Per-file summaries from a prepared diff: paths/status from the deltas;
/// addition/deletion counts and binary flags from a foreach line walk.
fn diff_summaries(diff: &mut git2::Diff) -> Vec<DiffFileSummary> {
    let mut files: Vec<DiffFileSummary> = Vec::new();
    for index in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(index) else {
            continue;
        };
        let status = delta.status();
        let old_file = delta.old_file();
        let new_file = delta.new_file();
        let Some(path) = new_file.path().or_else(|| old_file.path()) else {
            continue;
        };
        files.push(DiffFileSummary {
            // Untracked entries surface as plain adds.
            status: match status {
                git2::Delta::Added | git2::Delta::Untracked => "added".into(),
                git2::Delta::Deleted => "deleted".into(),
                git2::Delta::Renamed => "renamed".into(),
                git2::Delta::Copied => "copied".into(),
                _ => "modified".into(),
            },
            old_path: (status == git2::Delta::Renamed)
                .then(|| old_file.path().map(|path| path.display().to_string()))
                .flatten(),
            additions: 0,
            deletions: 0,
            binary: false,
            path: path.display().to_string(),
        });
    }
    let path_of = |delta: &git2::DiffDelta| -> String {
        delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    };
    // The foreach callbacks run one at a time on this thread; RefCell lets
    // both the binary and line walks annotate the shared summaries. The
    // walk is best-effort: on failure the summaries keep zero counts and
    // the patch text still carries the content.
    let files = std::cell::RefCell::new(files);
    let _ = diff.foreach(
        &mut |_delta, _progress| true,
        Some(&mut |delta, _binary| {
            if let Some(summary) = files
                .borrow_mut()
                .iter_mut()
                .find(|f| f.path == path_of(&delta))
            {
                summary.binary = true;
            }
            true
        }),
        None,
        Some(&mut |delta, _hunk, line| {
            let origin = line.origin();
            if (origin == '+' || origin == '-')
                && let Some(summary) = files
                    .borrow_mut()
                    .iter_mut()
                    .find(|f| f.path == path_of(&delta))
            {
                if origin == '+' {
                    summary.additions += 1;
                } else {
                    summary.deletions += 1;
                }
            }
            true
        }),
    );
    files.into_inner()
}

/// The full (uncapped) unified patch text of a prepared diff.
fn diff_patch_text(diff: &mut git2::Diff) -> String {
    let mut patch = String::new();
    let _ = diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        // libgit2 hands content lines without their +/-/space marker: the
        // origin must be re-prepended or the patch is not a valid unified
        // diff. Header ('F'), hunk ('H'), binary ('B'), and no-newline
        // marker lines arrive complete.
        match line.origin() {
            '+' | '-' | ' ' => {
                patch.push(line.origin());
                patch.push_str(&String::from_utf8_lossy(line.content()));
            }
            _ => patch.push_str(&String::from_utf8_lossy(line.content())),
        }
        true
    });
    patch
}

// ---- turn baseline (ADR-0003) ----

/// The net-change starting point of a chat's latest Turn, captured
/// synchronously when a queued command is accepted: the HEAD sha and the
/// uncommitted patch at turn start. In-memory and latest-per-chat; an
/// engine restart drops them.
#[derive(Debug, Clone)]
pub(crate) struct TurnBaseline {
    pub head_sha: String,
    pub patch: String,
}

/// One Turn's captured net change (ADR-0024): the typed files plus their
/// totals, ready for the `TurnChangeSet` wire shape.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnChangeCapture {
    pub files: Vec<TurnFileChange>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
}

/// The settle-time freeze of one Turn's change set (ADR-0024 ticket 02):
/// the summary and its immutable per-file content, captured from one
/// working-tree snapshot in one locked Git pass.
#[derive(Debug, Clone)]
pub(crate) struct TurnChangeFreeze {
    pub capture: TurnChangeCapture,
    pub content: Vec<TurnFileContent>,
}

/// One file's immutable before/after pair inside a settled Turn's persisted
/// change-set record (ADR-0024 ticket 02): the turn-start content on the old
/// side, the settle-time working-tree file on the new side. Binary entries
/// keep their hashes and carry no text; each side is capped at
/// [`MAX_FILE_SIDE_BYTES`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnFileContent {
    /// The file's current path — the rename destination.
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub old_content_hash: Option<String>,
    pub new_content_hash: Option<String>,
    pub binary: bool,
    pub truncated: bool,
}

/// Map a Git-derived summary to the change-set vocabulary. A detected
/// rename keeps its previous path; a move Git could not pair never reaches
/// here as one delta — it is already a separate delete plus add, the
/// fallback. A copy is a new file on its new-side path.
fn turn_file_change(file: &DiffFileSummary) -> TurnFileChange {
    let status = match file.status.as_str() {
        "added" => TurnFileChangeStatus::Added,
        "deleted" => TurnFileChangeStatus::Deleted,
        "renamed" => TurnFileChangeStatus::Renamed,
        "copied" => TurnFileChangeStatus::Added,
        _ => TurnFileChangeStatus::Modified,
    };
    TurnFileChange {
        path: file.path.clone(),
        old_path: (status == TurnFileChangeStatus::Renamed)
            .then(|| file.old_path.clone())
            .flatten(),
        status,
        additions: file.additions,
        deletions: file.deletions,
        binary: file.binary,
    }
}

/// One file's section of a unified patch: from its "diff --git" line to
/// the next (or EOF), keyed by the section's new-side path.
struct PatchSection {
    path: String,
    text: String,
}

/// Split a unified patch into per-file sections. The path comes from the
/// "+++ b/<path>" line (or "--- a/<path>" for deletions); exotic
/// space-bearing paths are not handled — the engine produces these patches
/// itself from ordinary agent file writes.
fn split_sections(patch: &str) -> Vec<PatchSection> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut sections = Vec::new();
    let mut start = None;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") && start.is_none() {
            start = Some(index);
        } else if line.starts_with("diff --git ") {
            sections.push(section_of(&lines[start.unwrap()..index]));
            start = Some(index);
        }
    }
    if let Some(start) = start {
        sections.push(section_of(&lines[start..]));
    }
    sections
}

fn section_of(lines: &[&str]) -> PatchSection {
    let path = lines
        .iter()
        .find_map(|line| line.strip_prefix("+++ b/"))
        .or_else(|| lines.iter().find_map(|line| line.strip_prefix("--- a/")))
        // Pure renames carry no ---/+++ pair; "rename to" names the file.
        .or_else(|| {
            lines
                .iter()
                .find_map(|line| line.strip_prefix("rename to "))
        })
        // Binary deltas carry neither pair — only the header. Take the
        // new-side path so a binary file still keys as a changed path
        // instead of vanishing from the net-change filter.
        .or_else(|| {
            lines.iter().find_map(|line| {
                line.strip_prefix("diff --git ")
                    .and_then(|rest| rest.rsplit_once(" b/"))
                    .map(|(_, path)| path)
            })
        })
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut text = lines.join("\n");
    text.push('\n');
    PatchSection { path, text }
}

/// The net-change filter (ADR-0003): keep only files whose CURRENT
/// uncommitted section differs from the turn-start baseline section —
/// pre-existing dirty files the Turn never touched drop out, and edits
/// that return a file to its turn-start state (net zero) drop out too.
/// Returns the filtered patch and the kept paths.
pub(crate) fn filter_turn_patch(baseline: &str, current: &str) -> (String, Vec<String>) {
    let baseline_sections: HashMap<String, String> = split_sections(baseline)
        .into_iter()
        .map(|section| (section.path, section.text))
        .collect();
    let mut kept_paths = Vec::new();
    let mut kept = String::new();
    for section in split_sections(current) {
        let untouched = baseline_sections
            .get(&section.path)
            .is_some_and(|baseline| *baseline == section.text);
        if untouched {
            continue;
        }
        kept_paths.push(section.path.clone());
        kept.push_str(&section.text);
    }
    (kept, kept_paths)
}

/// Reconstruct a file's turn-start content: the baseline section's hunks
/// applied over the HEAD blob ("HEAD blob where the file was clean").
/// Sections that mean absent-at-turn-start (adds, deletions) yield `None`.
/// Pure — the applicer only walks standard unified hunks.
pub(crate) fn turn_start_content(base: Option<&str>, section: &str) -> Option<String> {
    let lines: Vec<&str> = section.lines().collect();
    let added = lines.iter().any(|line| line.trim() == "--- /dev/null");
    let deleted = lines.iter().any(|line| line.trim() == "+++ /dev/null");
    if added || deleted {
        return None;
    }
    let base = base?;
    let base_lines: Vec<&str> = base.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut base_idx = 0usize;
    let mut in_hunk = false;
    for line in lines {
        if let Some(header) = line.strip_prefix("@@") {
            let old_start = header
                .split_whitespace()
                .find_map(|token| token.strip_prefix('-'))
                .and_then(|token| token.split(',').next())
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1);
            // Emit the untouched base lines before the hunk begins.
            while base_idx + 1 < old_start && base_idx < base_lines.len() {
                out.push(base_lines[base_idx].to_string());
                base_idx += 1;
            }
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with("diff --git ") {
            if line.starts_with("diff --git ") {
                in_hunk = false;
            }
            continue;
        }
        let mut chars = line.chars();
        let origin = chars.next();
        let body: String = chars.collect();
        match origin {
            Some(' ') => {
                if base_idx < base_lines.len() {
                    out.push(base_lines[base_idx].to_string());
                    base_idx += 1;
                } else {
                    out.push(body);
                }
            }
            Some('-') => {
                if base_idx < base_lines.len() {
                    base_idx += 1;
                }
            }
            Some('+') => out.push(body),
            _ => {} // '\' no-newline markers and anything else: skip
        }
    }
    while base_idx < base_lines.len() {
        out.push(base_lines[base_idx].to_string());
        base_idx += 1;
    }
    let mut content = out.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    Some(content)
}

/// One side of a per-file text pair: bytes, binary detection, and the 1 MiB
/// cap. Binary sides carry no text.
struct FileSide {
    text: Option<String>,
    content_hash: Option<String>,
    truncated: bool,
    binary: bool,
}

fn file_side(bytes: Option<Vec<u8>>) -> FileSide {
    let Some(bytes) = bytes else {
        return FileSide {
            text: None,
            content_hash: None,
            truncated: false,
            binary: false,
        };
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let content_hash = hex(&hasher.finalize());
    // git's heuristic: a NUL in the leading 8k bytes means binary.
    let binary = bytes.iter().take(8000).any(|byte| *byte == 0);
    if binary {
        return FileSide {
            text: None,
            content_hash: Some(content_hash),
            truncated: false,
            binary,
        };
    }
    let text = String::from_utf8_lossy(&bytes);
    let truncated = text.len() > MAX_FILE_SIDE_BYTES;
    let text = if truncated {
        let mut cut = MAX_FILE_SIDE_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text[..cut].to_string()
    } else {
        text.into_owned()
    };
    FileSide {
        text: Some(text),
        content_hash: Some(content_hash),
        truncated,
        binary,
    }
}

/// The "Latest turn" capture (ADR-0003): diff `HEAD@start → workdir with
/// index`, then keep only the files whose current section differs from the
/// baseline section — the net change since the Turn began.
fn turn_capture(
    repo: &Repository,
    device_id: &str,
    baseline: &TurnBaseline,
) -> Result<CheckoutDiff, GitFault> {
    let current_head = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id().to_string())
        .unwrap_or_else(|| EMPTY_HEAD.to_string());
    let base_tree = if baseline.head_sha == EMPTY_HEAD {
        None
    } else {
        let oid = git2::Oid::from_str(&baseline.head_sha)
            .map_err(|_| GitFault::Error("turn baseline commit not found".into()))?;
        Some(
            repo.find_commit(oid)
                .map_err(|_| GitFault::Error("turn baseline commit not found".into()))?
                .tree()
                .map_err(git_message)
                .map_err(GitFault::Error)?,
        )
    };

    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .show_binary(true);
    let mut diff = repo
        .diff_tree_to_workdir_with_index(base_tree.as_ref(), Some(&mut options))
        .map_err(git_message)
        .map_err(GitFault::Error)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).for_untracked(true);
    let _ = diff.find_similar(Some(&mut find));

    let mut files = diff_summaries(&mut diff);
    let current_patch = diff_patch_text(&mut diff);
    let (patch, kept) = filter_turn_patch(&baseline.patch, &current_patch);
    files.retain(|file| kept.contains(&file.path));
    let (patch, truncated) = truncate_patch(&patch, MAX_PATCH_BYTES);

    let additions: u32 = files.iter().map(|file| file.additions).sum();
    let deletions: u32 = files.iter().map(|file| file.deletions).sum();
    let workdir = repo
        .workdir()
        .map(|path| path.display().to_string().trim_end_matches('/').to_string())
        .unwrap_or_default();
    Ok(CheckoutDiff {
        // The CURRENT head folds in: a commit made during the turn re-keys
        // even when the net patch is unchanged.
        checksum: diff_checksum(&current_head, "turn", "", &patch),
        checkout_id: checkout_identity(device_id, repo.path()),
        device_id: device_id.to_string(),
        cwd: workdir,
        additions,
        deletions,
        patch,
        files,
        truncated,
        updated_at: chrono::Utc::now(),
    })
}

/// The turn-start side of one path: the file's TURN-START content (baseline
/// section applied over the HEAD blob; the HEAD blob where the file was
/// clean at turn start; absent when the file did not exist then).
fn turn_start_side(
    repo: &Repository,
    path: &str,
    baseline: &TurnBaseline,
) -> Result<FileSide, GitFault> {
    let path = Path::new(path);
    let base_tree = if baseline.head_sha == EMPTY_HEAD {
        None
    } else {
        let oid = git2::Oid::from_str(&baseline.head_sha)
            .map_err(|_| GitFault::Error("turn baseline commit not found".into()))?;
        Some(
            repo.find_commit(oid)
                .map_err(|_| GitFault::Error("turn baseline commit not found".into()))?
                .tree()
                .map_err(git_message)
                .map_err(GitFault::Error)?,
        )
    };
    let head_blob = base_tree.and_then(|tree| {
        tree.get_path(path)
            .ok()
            .and_then(|entry| entry.to_object(repo).ok())
            .and_then(|object| object.as_blob().map(|blob| blob.content().to_vec()))
    });
    let baseline_section = split_sections(&baseline.patch)
        .into_iter()
        .find(|section| section.path == path.display().to_string())
        .map(|section| section.text);
    let old_bytes: Option<Vec<u8>> = match &baseline_section {
        None => head_blob,
        Some(section) => turn_start_content(
            head_blob
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .as_deref(),
            section,
        )
        .map(String::into_bytes),
    };
    Ok(file_side(old_bytes))
}

/// The working-tree side of one path: the file's current bytes, absent when
/// it is deleted.
fn workdir_side(repo: &Repository, path: &str) -> FileSide {
    let new_bytes = repo
        .workdir()
        .map(|workdir| workdir.join(Path::new(path)))
        .and_then(|full| std::fs::read(full).ok());
    file_side(new_bytes)
}

/// Per-file text for the turn scope: old side is the file's TURN-START
/// content (baseline section applied over the HEAD blob; the HEAD blob
/// where the file was clean at turn start), new side the working-tree
/// file.
fn turn_file_text_blocking(
    repo: &Repository,
    request: &holt_proto::GetCheckoutFileDiffTextRequest,
    baseline: &TurnBaseline,
) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
    let old = turn_start_side(repo, &request.path, baseline)?;
    let new = workdir_side(repo, &request.path);

    Ok(holt_proto::CheckoutFileDiffText {
        diff_checksum: request.diff_checksum.clone(),
        old_text: old.text,
        new_text: new.text,
        old_content_hash: old.content_hash,
        new_content_hash: new.content_hash,
        binary: old.binary || new.binary,
        truncated: old.truncated || new.truncated,
        // A turn capture is live state: freshness is judged by the pinned
        // checksum against a fresh recompute, exactly like the other
        // workdir-based scopes.
        stale: false,
    })
}

/// Per-file text for the commit scope: parent blob vs commit blob, never
/// the working tree.
fn commit_file_text(
    repo: &Repository,
    request: &holt_proto::GetCheckoutFileDiffTextRequest,
) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
    let commit_sha = request
        .commit_sha
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| GitFault::BadParams("commitSha is required for commit diffs".into()))?;
    let commit = repo
        .revparse_single(commit_sha)
        .map_err(|_| GitFault::BadParams(format!("unknown commit: {commit_sha}")))?
        .peel_to_commit()
        .map_err(|_| GitFault::BadParams(format!("not a commit: {commit_sha}")))?;
    let path = Path::new(&request.path);
    let blob_from = |tree: Option<git2::Tree>| -> Option<Vec<u8>> {
        tree.and_then(|tree| {
            tree.get_path(path)
                .ok()
                .and_then(|entry| entry.to_object(repo).ok())
                .and_then(|object| object.as_blob().map(|blob| blob.content().to_vec()))
        })
    };
    let old_bytes = match commit.parent_count() {
        0 => None,
        _ => blob_from(commit.parent(0).ok().and_then(|parent| parent.tree().ok())),
    };
    let new_bytes = blob_from(commit.tree().ok());
    let old = file_side(old_bytes);
    let new = file_side(new_bytes);
    // A pinned commit pair cannot go stale by definition; the checksum is
    // echoed so the UI's keying stays stable.
    Ok(holt_proto::CheckoutFileDiffText {
        diff_checksum: request.diff_checksum.clone(),
        old_text: old.text,
        new_text: new.text,
        old_content_hash: old.content_hash,
        new_content_hash: new.content_hash,
        binary: old.binary || new.binary,
        truncated: old.truncated || new.truncated,
        stale: false,
    })
}

fn capture_file_text(
    repo: &Repository,
    device_id: &str,
    request: &holt_proto::GetCheckoutFileDiffTextRequest,
) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
    // Commit mode never reads the live working tree.
    if request.mode == "commit" {
        return commit_file_text(repo, request);
    }
    let path = Path::new(&request.path);
    let mode = if request.mode.is_empty() {
        "workingTree"
    } else {
        request.mode.as_str()
    };
    // Old side: the mode's base tree blob at this path (None for files the
    // base doesn't know — adds and renames).
    let head_tree = repo.head().ok().and_then(|head| head.peel_to_tree().ok());
    let base_tree = base_tree_for(repo, mode, request.base_ref.as_deref(), head_tree.as_ref())?;
    let old_bytes = base_tree.and_then(|tree| {
        tree.get_path(path)
            .ok()
            .and_then(|entry| entry.to_object(repo).ok())
            .and_then(|object| object.as_blob().map(|blob| blob.content().to_vec()))
    });
    // New side: the working-tree file (None when deleted).
    let new_bytes = repo
        .workdir()
        .map(|workdir| workdir.join(path))
        .and_then(|full| std::fs::read(full).ok());
    let old = file_side(old_bytes);
    let new = file_side(new_bytes);

    // Staleness: the capture the caller rendered is keyed by
    // `diff_checksum`; recompute the current key and compare. The HEAD
    // component means a commit alone re-keys even when the patch is
    // byte-identical.
    let current = capture_diff(repo, device_id, mode, request.base_ref.as_deref())?;
    let stale = request.diff_checksum != current.checksum;

    Ok(holt_proto::CheckoutFileDiffText {
        diff_checksum: request.diff_checksum.clone(),
        old_text: old.text,
        new_text: new.text,
        old_content_hash: old.content_hash,
        new_content_hash: new.content_hash,
        binary: old.binary || new.binary,
        truncated: old.truncated || new.truncated,
        stale,
    })
}

// ---- history ----

/// One page of the commit graph: topologically ordered from HEAD, with
/// branch/remote/tag refs attached to the commits they name.
fn history_page(
    repo: &Repository,
    cursor: usize,
    limit: usize,
) -> Result<holt_proto::GitHistoryPage, String> {
    let head_sha = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id().to_string());
    let Some(head_sha) = head_sha else {
        return Ok(holt_proto::GitHistoryPage {
            commits: Vec::new(),
            head_sha: None,
            next_cursor: None,
            total_count: Some(0),
            head_commit_count: Some(0),
        });
    };

    // Ref labels by target commit, resolved once for the whole page walk.
    let mut refs_by_commit: std::collections::HashMap<git2::Oid, Vec<holt_proto::GitHistoryRef>> =
        std::collections::HashMap::new();
    let references = repo.references().map_err(git_message)?;
    for reference in references.flatten() {
        let (kind, label) = if let Some(name) = reference
            .name()
            .ok()
            .and_then(|name| name.strip_prefix("refs/heads/"))
        {
            (holt_proto::GitHistoryRefKind::Branch, name.to_string())
        } else if let Some(name) = reference
            .name()
            .ok()
            .and_then(|name| name.strip_prefix("refs/remotes/"))
        {
            (holt_proto::GitHistoryRefKind::Remote, name.to_string())
        } else if let Some(name) = reference
            .name()
            .ok()
            .and_then(|name| name.strip_prefix("refs/tags/"))
        {
            (holt_proto::GitHistoryRefKind::Tag, name.to_string())
        } else {
            continue;
        };
        // Symbolic refs (origin/HEAD) peel to their target's commit, which
        // would duplicate labels; only direct refs name commits.
        if let Ok(target) = reference.peel_to_commit() {
            refs_by_commit
                .entry(target.id())
                .or_default()
                .push(holt_proto::GitHistoryRef { kind, label });
        }
    }

    let mut walker = repo.revwalk().map_err(git_message)?;
    walker
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .map_err(git_message)?;
    walker.push_head().map_err(git_message)?;
    // The page collects at most `limit` commits after the cursor, but the
    // walk runs to the end: the totals need the full count (oid walking is
    // cheap; only the page's commits get loaded).
    let mut commits = Vec::new();
    let mut visited = 0usize;
    let mut next_cursor = None;
    for oid in walker {
        let oid = oid.map_err(git_message)?;
        visited += 1;
        if visited <= cursor {
            continue;
        }
        if commits.len() >= limit {
            if next_cursor.is_none() {
                next_cursor = Some(visited - 1);
            }
            continue;
        }
        let commit = repo.find_commit(oid).map_err(git_message)?;
        let parent_shas: Vec<String> = (0..commit.parent_count())
            .filter_map(|index| commit.parent_id(index).ok().map(|oid| oid.to_string()))
            .collect();
        let author = commit.author();
        let authored_at = chrono::DateTime::from_timestamp(author.when().seconds(), 0)
            .map(|time| time.to_rfc3339())
            .unwrap_or_default();
        commits.push(holt_proto::GitHistoryCommit {
            sha: commit.id().to_string(),
            subject: commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string(),
            author_name: author.name().unwrap_or_default().to_string(),
            author_email: author.email().unwrap_or_default().to_string(),
            authored_at,
            refs: refs_by_commit
                .get(&commit.id())
                .cloned()
                .unwrap_or_default(),
            parent_shas,
        });
    }
    let total_count = visited;
    Ok(holt_proto::GitHistoryPage {
        commits,
        head_sha: Some(head_sha),
        next_cursor,
        total_count: Some(total_count),
        head_commit_count: Some(total_count),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PATCH_BYTES, checkout_identity, classify_status, default_branch, diff_checksum,
        filter_turn_patch, status_sides, truncate_patch, turn_start_content,
    };

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    // ---- truncate_patch ----

    fn file_section(path: &str, hunks: &[&str]) -> String {
        let mut out = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        for hunk in hunks {
            out.push_str(hunk);
        }
        out
    }

    #[test]
    fn truncate_patch_keeps_short_patches_whole() {
        let patch = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let (kept, truncated) = truncate_patch(patch, MAX_PATCH_BYTES);
        assert_eq!(kept, patch);
        assert!(!truncated);
    }

    #[test]
    fn truncate_patch_cuts_between_hunks_and_flags() {
        let first_hunk = "@@ -1,2 +1,2 @@\n line\n-old\n+new\n";
        let second_hunk = "@@ -100,2 +100,2 @@\n line\n-old\n+new\n";
        let patch = format!(
            "{}{}{}",
            file_section("x", &[first_hunk]),
            first_hunk,
            second_hunk
        );
        let cap = file_section("x", &[]).len() + first_hunk.len() * 2 - 10;
        let (kept, truncated) = truncate_patch(&patch, cap);
        assert!(truncated);
        // Cut lands between hunks: the tail is a complete hunk.
        assert!(kept.ends_with("+new\n"));
        assert!(kept.len() <= cap + first_hunk.len()); // first-segment progress rule
        assert!(!kept.contains(second_hunk.trim_end()));
    }

    #[test]
    fn truncate_patch_never_loses_everything() {
        // One hunk larger than the whole cap: the cut falls to a line
        // boundary inside it, keeping the cap and real content.
        let hunk = format!("@@ -1 +1 @@\n{}", "+line\n".repeat(500));
        let tail = format!("@@ -999 +999 @@\n{}", "-gone\n".repeat(500));
        let patch = format!("{}{}{}", file_section("x", &[]), hunk, tail);
        let header_len = file_section("x", &[]).len();
        let cap = header_len + 100;
        let (kept, truncated) = truncate_patch(&patch, cap);
        assert!(truncated);
        assert!(kept.starts_with("diff --git a/x b/x"));
        assert!(kept.contains("+line"));
        assert!(kept.len() <= cap);
        assert!(!kept.contains("-gone"));
    }

    #[test]
    fn truncate_patch_keeps_whole_files_when_they_fit() {
        let section = file_section("x", &["@@ -1 +1 @@\n-a\n+b\n"]);
        let patch = format!("{section}{section}");
        let (kept, truncated) = truncate_patch(&patch, section.len() * 2);
        assert_eq!(kept, patch);
        assert!(!truncated);
    }

    // ---- diff_checksum ----

    #[test]
    fn diff_checksum_is_deterministic_and_scope_sensitive() {
        let a = diff_checksum("sha1", "workingTree", "", "patch");
        assert_eq!(a, diff_checksum("sha1", "workingTree", "", "patch"));
        // HEAD folds in: same patch, new commit ⇒ new key.
        assert_ne!(a, diff_checksum("sha2", "workingTree", "", "patch"));
        // Mode and base ref fold in too.
        assert_ne!(a, diff_checksum("sha1", "branch", "main", "patch"));
        assert_ne!(
            diff_checksum("sha1", "branch", "main", "patch"),
            diff_checksum("sha1", "branch", "dev", "patch")
        );
        // Empty patch is a valid key.
        assert!(!diff_checksum("sha1", "workingTree", "", "").is_empty());
    }

    // ---- checkout_identity ----

    #[test]
    fn checkout_identity_separates_devices_dirs_and_worktrees() {
        let main = std::path::Path::new("/repo/.git");
        let worktree = std::path::Path::new("/repo/.git/worktrees/wt");
        assert_eq!(
            checkout_identity("device", main),
            checkout_identity("device", main)
        );
        assert_ne!(
            checkout_identity("device", main),
            checkout_identity("device", worktree),
            "a linked worktree is its own checkout"
        );
        assert_ne!(
            checkout_identity("device", main),
            checkout_identity("other", main),
            "the device is part of the identity"
        );
        // The NUL separator keeps "ab"+"c" distinct from "a"+"bc".
        assert_ne!(
            checkout_identity("ab", std::path::Path::new("c/.git")),
            checkout_identity("a", std::path::Path::new("bc/.git"))
        );
    }

    #[test]
    fn default_branch_prefers_origin_head_target() {
        let names = names(&["main", "trunk", "zeta"]);
        assert_eq!(default_branch(&names, Some("trunk")).unwrap(), "trunk");
    }

    #[test]
    fn default_branch_ignores_origin_head_target_missing_locally() {
        let names = names(&["develop", "main"]);
        assert_eq!(default_branch(&names, Some("trunk")).unwrap(), "main");
    }

    #[test]
    fn default_branch_falls_back_to_main_then_master() {
        assert_eq!(
            default_branch(&names(&["master", "feature/x"]), None).unwrap(),
            "master"
        );
        assert_eq!(
            default_branch(&names(&["feature/x", "master", "main"]), None).unwrap(),
            "main"
        );
    }

    #[test]
    fn default_branch_falls_back_to_alphabetical_first() {
        assert_eq!(
            default_branch(&names(&["zeta", "beta", "gamma"]), None).unwrap(),
            "beta"
        );
        assert_eq!(default_branch(&[], None), None);
    }

    // ---- working-tree status classification (file-sidebar ticket 10) ----

    #[test]
    fn classify_status_maps_git_flags_to_the_marker_set() {
        use holt_proto::WorkspaceGitStatusKind as Kind;
        assert_eq!(classify_status(git2::Status::IGNORED), Some(Kind::Ignored));

        let staged_new = git2::Status::INDEX_NEW | git2::Status::WT_MODIFIED;
        assert_eq!(classify_status(staged_new), Some(Kind::Added));

        assert_eq!(classify_status(git2::Status::WT_NEW), Some(Kind::Untracked));

        assert_eq!(
            classify_status(git2::Status::WT_MODIFIED),
            Some(Kind::Modified)
        );
        assert_eq!(
            classify_status(git2::Status::INDEX_MODIFIED),
            Some(Kind::Modified)
        );
        // A conflicted file reports Conflicted — never folded into Modified.
        assert_eq!(
            classify_status(git2::Status::CONFLICTED),
            Some(Kind::Conflicted)
        );
        assert_eq!(
            classify_status(git2::Status::CONFLICTED | git2::Status::WT_MODIFIED),
            Some(Kind::Conflicted)
        );
        // Removals report Deleted — invisible rows in a live tree, but the
        // kind keeps the snapshot honest.
        assert_eq!(
            classify_status(git2::Status::WT_DELETED),
            Some(Kind::Deleted)
        );
        assert_eq!(
            classify_status(git2::Status::INDEX_DELETED),
            Some(Kind::Deleted)
        );

        // Clean and bare index states carry no marker.
        assert_eq!(classify_status(git2::Status::CURRENT), None);
    }

    #[test]
    fn status_sides_report_index_and_worktree_independently() {
        use holt_proto::WorkspaceGitStatusKind as Kind;
        // MM: modified, staged, modified again — both halves report.
        assert_eq!(
            status_sides(git2::Status::INDEX_MODIFIED | git2::Status::WT_MODIFIED),
            (Some(Kind::Modified), Some(Kind::Modified))
        );
        // AM: staged new file, then modified — Added + Modified.
        assert_eq!(
            status_sides(git2::Status::INDEX_NEW | git2::Status::WT_MODIFIED),
            (Some(Kind::Added), Some(Kind::Modified))
        );
        // Plain untracked: worktree side only.
        assert_eq!(
            status_sides(git2::Status::WT_NEW),
            (None, Some(Kind::Untracked))
        );
        // Staged add, worktree clean: index side only.
        assert_eq!(
            status_sides(git2::Status::INDEX_NEW),
            (Some(Kind::Added), None)
        );
        // Deleted on one side only stays on that side.
        assert_eq!(
            status_sides(git2::Status::WT_DELETED),
            (None, Some(Kind::Deleted))
        );
        // Conflicted reports on both sides, like porcelain's UU/AA/DD.
        assert_eq!(
            status_sides(git2::Status::CONFLICTED | git2::Status::WT_MODIFIED),
            (Some(Kind::Conflicted), Some(Kind::Conflicted))
        );
        // Ignored and clean carry no sides.
        assert_eq!(status_sides(git2::Status::IGNORED), (None, None));
        assert_eq!(status_sides(git2::Status::CURRENT), (None, None));
    }

    // ---- turn net-change filter ----

    fn modified_section(path: &str, old: &str, new: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nindex 111..222 100644\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n"
        )
    }

    fn added_section(path: &str, content: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{content}\n"
        )
    }

    fn deleted_section(path: &str, content: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n@@ -1 +0,0 @@\n-{content}\n"
        )
    }

    fn renamed_section(old: &str, new: &str, content: &str) -> String {
        format!(
            "diff --git a/{old} b/{new}\nsimilarity index 90%\nrename from {old}\nrename to {new}\n--- a/{old}\n+++ b/{new}\n@@ -1 +1 @@\n-{content}\n+{content} edited\n"
        )
    }

    #[test]
    fn turn_filter_keeps_touched_and_new_drops_untouched_and_net_zero() {
        let baseline = format!(
            "{}{}",
            modified_section("dirty.txt", "user edit", "user edited"),
            modified_section("stable.txt", "s", "stable"),
        );
        // The turn never touched dirty.txt (section byte-identical) and
        // reverted its own edit to net.txt; it did touch agent.txt.
        let current = format!(
            "{}{}{}",
            modified_section("dirty.txt", "user edit", "user edited"),
            modified_section("net.txt", "a", "agent then reverted"),
            modified_section("agent.txt", "clean", "agent work"),
        );
        let baseline_net = modified_section("net.txt", "a", "agent then reverted");
        let baseline = format!("{baseline}{baseline_net}");

        let (patch, kept) = filter_turn_patch(&baseline, &current);
        assert_eq!(kept, ["agent.txt"]);
        assert!(patch.contains("agent.txt"));
        assert!(patch.contains("+agent work"));
        assert!(!patch.contains("dirty.txt"));
        assert!(!patch.contains("net.txt"));
    }

    #[test]
    fn turn_filter_keys_a_binary_section_by_its_header_path() {
        // Binary deltas carry neither a ---/+++ pair nor a rename line:
        // only the `diff --git a/x b/x` header names the file. Keying off
        // it keeps the binary file in the net-change filter instead of
        // dropping it as an empty path.
        let binary = "diff --git a/image.bin b/image.bin\nnew file mode 100644\nindex 000..111\nBinary files /dev/null and b/image.bin differ\n";
        let (patch, kept) = filter_turn_patch("", binary);
        assert_eq!(kept, ["image.bin"]);
        assert!(patch.contains("Binary files"));

        let (patch, kept) = filter_turn_patch(binary, binary);
        assert!(kept.is_empty(), "an untouched binary drops: {patch}");
    }

    #[test]
    fn turn_filter_keeps_new_deleted_and_renamed_files() {
        let current = format!(
            "{}{}{}",
            added_section("new.txt", "fresh"),
            deleted_section("gone.txt", "old"),
            renamed_section("from.rs", "to.rs", "code"),
        );
        let (patch, kept) = filter_turn_patch("", &current);
        assert_eq!(kept, ["new.txt", "gone.txt", "to.rs"]);
        assert!(patch.contains("+fresh"));
        assert!(patch.contains("-old"));
        assert!(patch.contains("rename from from.rs"));
    }

    #[test]
    fn turn_filter_renamed_away_drops_when_the_rename_predates_the_turn() {
        // The rename already existed at turn start: identical sections drop.
        let baseline = renamed_section("from.rs", "to.rs", "code");
        let (patch, kept) = filter_turn_patch(&baseline, &baseline);
        assert!(kept.is_empty());
        assert!(patch.is_empty());
    }

    // ---- turn-start content reconstruction ----

    #[test]
    fn turn_start_content_applies_baseline_hunks_over_the_head_blob() {
        // The file was dirty at turn start: HEAD says "line", the baseline
        // section shows the turn-start state as the agent found it.
        let section = "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1,2 +1,3 @@\n keep\n-was\n+became\n+extra\n";
        let head = "keep\nwas\ntail\n";
        assert_eq!(
            turn_start_content(Some(head), section).as_deref(),
            Some("keep\nbecame\nextra\ntail\n")
        );
    }

    #[test]
    fn turn_start_content_cleans_yield_the_head_blob() {
        // Clean at turn start (no baseline section): the HEAD blob is the
        // turn-start content. Passing None as the section models that.
        assert_eq!(
            turn_start_content(Some("head state\n"), "").as_deref(),
            Some("head state\n")
        );
    }

    #[test]
    fn turn_start_content_absent_files_yield_none() {
        let added = added_section("new.txt", "fresh");
        assert_eq!(turn_start_content(None, &added), None);
        let deleted = deleted_section("gone.txt", "old");
        assert_eq!(turn_start_content(Some("old\n"), &deleted), None);
    }
}
