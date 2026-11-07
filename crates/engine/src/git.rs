//! The engine's git capability on the `git2` backend (ADR-git2-as-the-git-backend).
//!
//! This module is the only place in the workspace that touches git2. Every
//! git2 call runs on the blocking pool — never on the async runtime's thread —
//! and a per-checkout lock, keyed by the repository's common git dir,
//! serializes operations against the same repository while different
//! repositories proceed in parallel.

mod checkpoints;

pub(crate) use checkpoints::{CallStart, CheckpointFile, CheckpointSide, WindowStart};

use std::{
    collections::{HashMap, HashSet},
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

const SESSION_BRANCH_ADJECTIVES: [&str; 32] = [
    "amber", "brave", "bright", "calm", "clever", "crisp", "eager", "gentle", "golden", "happy",
    "humble", "keen", "kind", "lively", "lucky", "merry", "nimble", "noble", "polite", "proud",
    "quick", "quiet", "shiny", "silent", "smart", "steady", "sunny", "swift", "tidy", "warm",
    "wise", "witty",
];
const SESSION_BRANCH_NOUNS: [&str; 32] = [
    "badger", "bear", "bird", "bison", "cat", "crane", "crow", "deer", "dolphin", "dove", "eagle",
    "falcon", "finch", "fox", "gecko", "hare", "hawk", "heron", "koala", "lemur", "lion", "lynx",
    "otter", "owl", "panda", "puma", "raven", "robin", "seal", "sparrow", "tiger", "wolf",
];

/// A readable branch name derived from the chat id, stable across admissions
/// and restarts without another persisted field. The digest suffix keeps
/// identical word pairs distinct.
fn session_branch_name(chat_id: &str) -> String {
    let digest = Sha256::digest(chat_id.as_bytes());
    let adjective = SESSION_BRANCH_ADJECTIVES[digest[0] as usize % SESSION_BRANCH_ADJECTIVES.len()];
    let noun = SESSION_BRANCH_NOUNS[digest[1] as usize % SESSION_BRANCH_NOUNS.len()];
    format!("holt/{adjective}-{noun}-{}", hex(&digest[2..6]))
}

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

    /// The integration view's snapshot ([`integration_snapshot`]): the
    /// trunk's commits plus every linked worktree's position, all under one
    /// hold of the checkout lock.
    pub(crate) async fn integration(
        &self,
        repo_path: &str,
        trunk: Option<String>,
        checks_root: PathBuf,
    ) -> Result<holt_proto::IntegrationSnapshot, String> {
        self.with_repo(repo_path, move |repo| {
            integration_snapshot(&repo, trunk.as_deref(), &checks_root)
        })
        .await
    }

    /// `PreviewMerge`: the in-memory squash or fast-forward and its diff,
    /// with a typed refusal instead of an error. Never writes.
    pub(crate) async fn preview_merge(
        &self,
        repo_path: &str,
        worktree: String,
        trunk: String,
        mode: String,
        title: Option<String>,
    ) -> Result<holt_proto::MergePreview, String> {
        self.with_repo(repo_path, move |repo| {
            merge_preview(&repo, &worktree, &trunk, &mode, title.as_deref())
        })
        .await
    }

    /// `MergeIntoTrunk`: the whole landing under the checkout lock. A
    /// typed refusal arrives as a `Failed` whose first line is the stable
    /// `merge-refused: <reason>` marker.
    pub(crate) async fn merge_into_trunk(
        &self,
        repo_path: &str,
        worktree: String,
        trunk: String,
        mode: String,
        message: Option<String>,
        expect: holt_proto::MergeHeads,
    ) -> Result<holt_proto::MergeResult, String> {
        self.with_repo(repo_path, move |repo| {
            merge_into_trunk(&repo, &worktree, &trunk, &mode, message, &expect)
                .map_err(merge_stop_to_string)
        })
        .await
    }

    /// `DeleteWorktree`'s git half ([`remove_worktree`]); the chat and Space
    /// bookkeeping lives in the RPC layer, which knows both.
    pub(crate) async fn remove_worktree(
        &self,
        repo_path: &str,
        worktree: String,
        force: bool,
    ) -> Result<RemovedWorktree, String> {
        self.with_repo(repo_path, move |repo| {
            remove_worktree(&repo, &worktree, force)
        })
        .await
    }

    /// The trunk's `.holt/checks.toml` blob, read from the trunk HEAD tree —
    /// never the working tree, so a branch cannot edit its own checks. `None`
    /// when the trunk declares none.
    pub(crate) async fn checks_config_blob(
        &self,
        repo_path: &str,
        trunk: String,
    ) -> Result<Option<Vec<u8>>, String> {
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let trunk_commit = main_repo
                .find_reference(&format!("refs/heads/{trunk}"))
                .and_then(|reference| reference.peel_to_commit())
                .map_err(|_| format!("trunk {trunk} does not resolve"))?;
            let tree = trunk_commit.tree().map_err(git_message)?;
            let Some(entry) = tree
                .get_path(std::path::Path::new(".holt/checks.toml"))
                .ok()
                .map(|entry| entry.id())
            else {
                return Ok(None);
            };
            let blob = main_repo.find_blob(entry).map_err(git_message)?;
            Ok(Some(blob.content().to_vec()))
        })
        .await
    }

    pub(crate) async fn checks_merge_tree(
        &self,
        repo_path: &str,
        worktree: String,
        trunk: String,
    ) -> Result<ChecksMerge, String> {
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let context = match merge_context(&main_repo, &worktree, &trunk, "squash") {
                Ok(context) => context,
                Err(MergeStop::Refusal(reason, detail)) => {
                    // A conflicting or empty merge is the run's typed error,
                    // not the caller's.
                    return Err(merge_refusal_message(reason, &detail));
                }
                Err(MergeStop::Failed(message)) => return Err(message),
            };
            let heads = ChecksMerge {
                tree: String::new(),
                trunk_head: context.trunk_head.to_string(),
                branch_head: context.branch_head.to_string(),
            };
            match merged_tree(&main_repo, &context, "squash") {
                Ok(Ok(tree)) => Ok(ChecksMerge {
                    tree: tree.to_string(),
                    ..heads
                }),
                // Conflicting paths: the run's error names them.
                Ok(Err(paths)) => Err(format!("the merge preview conflicts: {}", paths.join(", "))),
                Err(MergeStop::Refusal(reason, detail)) => {
                    Err(merge_refusal_message(reason, &detail))
                }
                Err(MergeStop::Failed(message)) => Err(message),
            }
        })
        .await
    }

    /// A registered worktree's path and its checked-out branch (short
    /// name; `None` on a detached HEAD).
    pub(crate) async fn worktree_path_and_branch(
        &self,
        repo_path: &str,
        worktree: &str,
    ) -> Result<(String, Option<String>), String> {
        let worktree = worktree.to_string();
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let path = registered_worktree_path(&main_repo, &worktree)?;
            let branch = Repository::open(&path).ok().and_then(|worktree_repo| {
                let head = worktree_repo.head().ok()?;
                if !head.is_branch() {
                    return None;
                }
                head.shorthand().ok().map(str::to_string)
            });
            Ok((path.display().to_string(), branch))
        })
        .await
    }

    /// The reviewer names declared on the trunk's `.holt/reviewers/`
    /// directory (`.md` files, sorted). `None` when the repo declares none.
    pub(crate) async fn reviewers_in_trunk(
        &self,
        repo_path: &str,
        trunk: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let trunk = trunk.to_string();
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let trunk_commit = main_repo
                .find_reference(&format!("refs/heads/{trunk}"))
                .and_then(|reference| reference.peel_to_commit())
                .map_err(|_| format!("trunk {trunk} does not resolve"))?;
            let tree = trunk_commit.tree().map_err(git_message)?;
            let Ok(entry) = tree.get_path(std::path::Path::new(".holt/reviewers")) else {
                return Ok(None);
            };
            let subtree = main_repo.find_tree(entry.id()).map_err(git_message)?;
            let mut names: Vec<String> = (0..subtree.len())
                .filter_map(|index| subtree.get(index))
                .filter_map(|entry| {
                    entry
                        .name()
                        .ok()
                        .and_then(|name| name.strip_suffix(".md"))
                        .map(str::to_string)
                })
                .collect();
            names.sort();
            Ok(Some(names))
        })
        .await
    }

    /// One reviewer's prompt text: the trunk's `.holt/reviewers/<name>.md`
    /// blob, `None` when the repo does not define that reviewer.
    pub(crate) async fn reviewer_prompt(
        &self,
        repo_path: &str,
        trunk: &str,
        reviewer: &str,
    ) -> Result<Option<String>, String> {
        let trunk = trunk.to_string();
        let reviewer = reviewer.to_string();
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let trunk_commit = main_repo
                .find_reference(&format!("refs/heads/{trunk}"))
                .and_then(|reference| reference.peel_to_commit())
                .map_err(|_| format!("trunk {trunk} does not resolve"))?;
            let tree = trunk_commit.tree().map_err(git_message)?;
            let path = format!(".holt/reviewers/{reviewer}.md");
            let Some(entry) = tree
                .get_path(std::path::Path::new(&path))
                .ok()
                .map(|entry| entry.id())
            else {
                return Ok(None);
            };
            let blob = main_repo.find_blob(entry).map_err(git_message)?;
            Ok(Some(String::from_utf8_lossy(blob.content()).into_owned()))
        })
        .await
    }

    /// The trunk's and a worktree branch's current heads — the staleness pair
    /// a checks run is judged against. Read-only; no merge preconditions.
    pub(crate) async fn worktree_and_trunk_heads(
        &self,
        repo_path: &str,
        worktree: &str,
        trunk: &str,
    ) -> Result<(String, String), String> {
        let worktree = worktree.to_string();
        let trunk = trunk.to_string();
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let trunk_head = main_repo
                .find_reference(&format!("refs/heads/{trunk}"))
                .and_then(|reference| reference.peel_to_commit())
                .map_err(|_| format!("trunk {trunk} does not resolve"))?
                .id()
                .to_string();
            let worktree_path = main_repo
                .find_worktree(&worktree)
                .map_err(|_| format!("worktree {worktree} is not registered"))?
                .path()
                .to_path_buf();
            let branch_head = Repository::open(&worktree_path)
                .map_err(git_message)?
                .head()
                .and_then(|head| head.peel_to_commit())
                .map_err(|_| format!("worktree {worktree} has no HEAD"))?
                .id()
                .to_string();
            Ok((trunk_head, branch_head))
        })
        .await
    }

    /// Check `tree` out as a detached HEAD in a throwaway linked worktree at
    /// `path` (which must not exist; its parent is created). The tree lands
    /// through a temp commit on a temp branch — git2's `WorktreeAddOptions`
    /// needs a branch — and the branch is deleted once the worktree stands, so
    /// no ref is left behind while the checkout lives.
    pub(crate) async fn checkout_detached_tree(
        &self,
        repo_path: &str,
        name: &str,
        tree_sha: &str,
        trunk_head: &str,
        path: &str,
    ) -> Result<(), String> {
        let name = name.to_string();
        let tree_sha = tree_sha.to_string();
        let trunk_head = trunk_head.to_string();
        let path = path.to_string();
        self.with_repo(repo_path, move |repo| {
            let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
            let tree = main_repo
                .find_tree(git2::Oid::from_str(&tree_sha).map_err(git_message)?)
                .map_err(git_message)?;
            let parent = main_repo
                .find_commit(
                    git2::Oid::from_str(&trunk_head)
                        .map_err(|_| "trunk head is not a sha".to_string())?,
                )
                .map_err(git_message)?;
            let signature = main_repo.signature().map_err(|_| {
                "could not read the git identity (user.name/user.email)".to_string()
            })?;
            // Object-store only: no ref moves while the checkout lives.
            let commit = main_repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    "holt: checks checkout",
                    &tree,
                    &[&parent],
                )
                .map_err(git_message)?;
            let branch = format!("refs/heads/holt/check/{name}");
            main_repo
                .reference(&branch, commit, true, "holt: checks checkout")
                .map_err(git_message)?;
            // From here on, every failure unwinds what the branch's
            // existence created — the temp branch, and the worktree once
            // it is added.
            let outcome = checkout_detached_tree_finish(
                &main_repo,
                &name,
                &branch,
                commit,
                std::path::Path::new(&path),
            );
            if outcome.is_err() {
                let _ = main_repo.find_reference(&branch).map(|mut r| r.delete());
            }
            outcome
        })
        .await
    }

    /// The checks store's checkout key: `sha256` of the normalized common dir —
    /// one bucket per repository, not per checkout.
    pub(crate) async fn checkout_key(&self, repo_path: &str) -> Result<String, String> {
        self.with_repo(repo_path, move |repo| {
            Ok(sha256_hex(
                normalize(repo.commondir()).to_string_lossy().as_bytes(),
            ))
        })
        .await
    }

    /// Remove a checks worktree: its directory, then its registration.
    pub(crate) async fn remove_check_worktree(
        &self,
        repo_path: &str,
        name: &str,
    ) -> Result<(), String> {
        let name = name.to_string();
        self.with_repo(repo_path, move |repo| {
        let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
        let Ok(worktree) = main_repo.find_worktree(&name) else {
            return Ok(());
        };
        let path = worktree.path().to_path_buf();
        if path.is_dir()
            && let Err(error) = std::fs::remove_dir_all(&path)
        {
            tracing::warn!(path = %path.display(), %error, "could not remove the checks worktree");
        }
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).locked(true).working_tree(true);
        worktree
            .prune(Some(&mut prune))
            .map_err(|error| format!("could not prune the checks worktree: {error}"))?;
        Ok(())
    })
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
        // the listing, so the prefix the UI strips must be too — and in the
        // addressed vocabulary (`C:/…`, never Windows's verbatim `\\?\C:\…`)
        // so the string comparison survives on every platform.
        let workdir = workdir.canonicalize().unwrap_or(workdir);
        let display = crate::tools::addressed_canonical(&workdir);
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

    /// Capture a turn baseline (ADR-turn-diff-net-change-baseline): `(HEAD sha, uncommitted patch)`
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

    /// The Turn identity stamp (ADR-branch-is-live-working-tree-state): the working directory's live HEAD
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
    /// refetch. `attributed` narrows the result to the Turn's own write
    /// set (`None` = no narrowing); without it another chat's concurrent
    /// work in the same working tree would land in this Turn's scope.
    pub(crate) async fn turn_diff(
        &self,
        repo_path: &str,
        device_id: &str,
        baseline: &TurnBaseline,
        attributed: Option<&HashSet<String>>,
    ) -> Result<CheckoutDiff, GitFault> {
        let device_id = device_id.to_string();
        let baseline = baseline.clone();
        let attributed = attributed.cloned();
        self.with_repo(repo_path, move |repo| {
            turn_capture(&repo, &device_id, &baseline, attributed.as_ref())
        })
        .await
    }

    /// Per-file text for the turn scope; see [`turn_file_text_blocking`].
    /// `stale` is computed against a fresh turn recompute — filtered by the
    /// same attribution, so the checksum matches what the UI saw.
    pub(crate) async fn turn_file_text(
        &self,
        repo_path: &str,
        device_id: &str,
        request: &holt_proto::GetCheckoutFileDiffTextRequest,
        baseline: &TurnBaseline,
        attributed: Option<&HashSet<String>>,
    ) -> Result<holt_proto::CheckoutFileDiffText, GitFault> {
        let device_id = device_id.to_string();
        let request = request.clone();
        let baseline = baseline.clone();
        let attributed = attributed.cloned();
        self.with_repo(repo_path, move |repo| {
            let mut text = turn_file_text_blocking(&repo, &request, &baseline)?;
            let current = turn_capture(&repo, &device_id, &baseline, attributed.as_ref())?;
            text.stale = request.diff_checksum != current.checksum;
            Ok(text)
        })
        .await
    }

    /// The Turn change set (ADR-turn-change-sets-and-review): the net change from a Turn's
    /// baseline to the live working tree, as the typed vocabulary the Turn
    /// card and its Review render. Reuses the turn capture's Git path —
    /// dirty-start net-change filtering, rename detection, and the patch
    /// cap — and re-keys nothing: the caller adds Turn identity and phase.
    /// `attributed` is the Turn's own write set; files outside it are
    /// another writer's (another chat in the same working tree, or nothing
    /// in the Turn ran) and drop out of the card.
    pub(crate) async fn turn_change_capture(
        &self,
        repo_path: &str,
        device_id: &str,
        baseline: &TurnBaseline,
        attributed: &HashSet<String>,
    ) -> Result<TurnChangeCapture, GitFault> {
        let diff = self
            .turn_diff(repo_path, device_id, baseline, Some(attributed))
            .await?;
        let mut files: Vec<TurnFileChange> = diff.files.iter().map(turn_file_change).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(TurnChangeCapture {
            files,
            additions: diff.additions,
            deletions: diff.deletions,
            truncated: diff.truncated,
        })
    }

    /// The repo-root-relative form of `path` — the write/edit tools' target
    /// as the attribution vocabulary, which is the capture's path vocabulary
    /// (git paths are worktree-root-relative). Accepts absolute paths and
    /// paths relative to the chat's working directory; `None` when the path
    /// escapes the work tree or the folder is not a work tree — nothing the
    /// capture could ever contain.
    pub(crate) async fn repo_relative(&self, repo_path: &str, path: &str) -> Option<String> {
        let repo_root = repo_path.to_string();
        let path = path.to_string();
        self.with_repo(repo_path, move |repo| -> Result<Option<String>, String> {
            Ok(relative_path(&repo, &repo_root, &path))
        })
        .await
        .ok()
        .flatten()
    }

    /// The attribution scan behind a bash command's before/after window:
    /// path → current uncommitted patch-section text (untracked included),
    /// the same shape the net-change filter compares. A failed scan yields
    /// an empty map — the command then attributes nothing, never wrongly
    /// everything.
    ///
    /// With `store`, the window also keeps every uncommitted path's before
    /// side in the chat's checkpoint store
    /// (ADR-step-checkpoints-in-a-chat-owned-object-store).
    pub(crate) async fn window_start(
        &self,
        repo_path: &str,
        store: Option<PathBuf>,
    ) -> WindowStart {
        self.with_repo(repo_path, move |repo| -> Result<WindowStart, String> {
            let sections = workdir_sections(&repo).map_err(GitFault::into_string)?;
            Ok(checkpoints::window_start(&repo, sections, store.as_deref()))
        })
        .await
        .unwrap_or_else(|_| WindowStart::empty())
    }

    /// The changed sections now — a bash window's end.
    pub(crate) async fn changed_sections(&self, repo_path: &str) -> HashMap<String, String> {
        self.with_repo(repo_path, move |repo| workdir_sections(&repo))
            .await
            .unwrap_or_default()
    }

    /// A bash window's step files: `paths` (the window's attributed delta)
    /// with their before side from the window start and their after side
    /// stored now. `None` when the window has no trustworthy before side.
    pub(crate) async fn window_checkpoint(
        &self,
        repo_path: &str,
        store: PathBuf,
        window: WindowStart,
        paths: Vec<String>,
    ) -> Result<Option<Vec<CheckpointFile>>, String> {
        self.with_repo(repo_path, move |repo| {
            checkpoints::window_files(&repo, &store, &window, &paths).map_err(git_message)
        })
        .await
    }

    /// A write/edit target's repo-relative path and its current side,
    /// stored; `None` when the path is outside the work tree.
    pub(crate) async fn path_checkpoint(
        &self,
        repo_path: &str,
        store: PathBuf,
        path: &str,
    ) -> Result<Option<(String, CheckpointSide)>, String> {
        let repo_root = repo_path.to_string();
        let path = path.to_string();
        self.with_repo(repo_path, move |repo| {
            let Some(relative) = relative_path(&repo, &repo_root, &path) else {
                return Ok(None);
            };
            Ok(checkpoints::path_side(&repo, &store, &relative)
                .map_err(git_message)?
                .map(|side| (relative, side)))
        })
        .await
    }

    /// The immutable per-file before/after content of a frozen change set
    /// (ADR-turn-change-sets-and-review ticket 02), captured once at Turn settlement: the old side
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
        attributed: &HashSet<String>,
    ) -> Result<TurnChangeFreeze, GitFault> {
        let device_id = device_id.to_string();
        let baseline = baseline.clone();
        let attributed = attributed.clone();
        self.with_repo(repo_path, move |repo| {
            let diff = turn_capture(&repo, &device_id, &baseline, Some(&attributed))?;
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

    /// The work tree root of the repository containing `repo_path` — the
    /// base that a Turn record's repo-relative paths resolve against.
    pub(crate) async fn workdir(&self, repo_path: &str) -> Result<PathBuf, String> {
        let path = repo_path.to_string();
        tokio::task::spawn_blocking(move || {
            let repo = Repository::discover(&path).map_err(git_message)?;
            repo.workdir()
                .map(Path::to_path_buf)
                .ok_or_else(|| "the repository has no work tree".to_string())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// Whether `repo_path` resolves to a Git work tree. The change set's
    /// non-Git answer keys on this, never on a capture error.
    pub(crate) async fn is_work_tree(&self, repo_path: &str) -> bool {
        let path = repo_path.to_string();
        tokio::task::spawn_blocking(move || Repository::discover(&path).is_ok())
            .await
            .unwrap_or(false)
    }

    /// Materialize (or reuse) a chat's session worktree (ADR-session-worktrees-are-chat-owned-isolation): a linked
    /// worktree of `repo_path` at `<worktrees_root>/<chat_id>` on a readable
    /// `holt/<adjective>-<noun>-<short_id>` branch. The base ref applies only when the branch must
    /// be created; an existing, correctly-registered worktree is reused as-is —
    /// dirty files, extra commits, and a user's branch switch are never
    /// undone, and the creation branch may be long gone. Registration damage
    /// is reported, never auto-deleted. Returns the worktree path.
    pub(crate) async fn materialize_worktree(
        &self,
        repo_path: &str,
        base: &str,
        chat_id: &str,
        worktrees_root: &Path,
    ) -> Result<String, String> {
        if !crate::store::id_is_path_safe(chat_id) {
            return Err("chat id is not safe for a worktree path".into());
        }
        let wt_path = worktrees_root.join(chat_id);
        let base = base.to_string();
        let chat_id = chat_id.to_string();
        // git_worktree_add does not create intermediate directories.
        std::fs::create_dir_all(worktrees_root).map_err(|error| {
            format!(
                "could not create the worktrees root {}: {error}",
                worktrees_root.display()
            )
        })?;
        self.with_repo(repo_path, move |repo| {
            let common = normalize(repo.commondir());
            // A directory already at the target path must be this chat's own
            // registered worktree of THIS repo — anything else is a collision
            // a human has to resolve (the files stay put either way).
            if wt_path.exists() {
                return match Repository::open(&wt_path) {
                    Ok(wt_repo) if normalize(wt_repo.commondir()) != common => Err(format!(
                        "session worktree path {} is registered to another repository ({})",
                        wt_path.display(),
                        normalize(wt_repo.commondir()).display()
                    )),
                    Ok(_) => match repo.find_worktree(&chat_id) {
                        Ok(wt) if same_path(wt.path(), &wt_path) => {
                            Ok(wt_path.display().to_string())
                        }
                        Ok(wt) => Err(format!(
                            "session worktree {chat_id} is registered at an unexpected path ({})",
                            wt.path().display()
                        )),
                        Err(_) => Err(format!(
                            "session worktree path {} is not registered for chat {chat_id}",
                            wt_path.display()
                        )),
                    },
                    Err(error) => Err(format!(
                        "session worktree path {} exists but is not a usable work tree ({error})",
                        wt_path.display()
                    )),
                };
            }
            // Stale registration with a missing directory (crash, manual rm):
            // prune the metadata so re-creation starts clean.
            if let Ok(wt) = repo.find_worktree(&chat_id) {
                if wt.path().exists() {
                    return Err(format!(
                        "session worktree {chat_id} is registered at an unexpected path ({})",
                        wt.path().display()
                    ));
                }
                wt.prune(None).map_err(git_message)?;
            }
            let generated_branch = session_branch_name(&chat_id);
            let legacy_branch = format!("holt/{chat_id}");
            let branch_name = if repo
                .find_branch(&generated_branch, git2::BranchType::Local)
                .is_ok()
            {
                generated_branch
            } else if repo
                .find_branch(&legacy_branch, git2::BranchType::Local)
                .is_ok()
            {
                // Preserve branches created before readable names were introduced.
                legacy_branch
            } else {
                let commit = repo
                    .revparse_single(&base)
                    .map_err(|error| format!("base ref {base} does not resolve: {error}"))?
                    .peel_to_commit()
                    .map_err(|error| format!("base ref {base} is not a commit: {error}"))?;
                repo.branch(&generated_branch, &commit, false)
                    .map_err(git_message)?;
                generated_branch
            };
            let reference = repo
                .find_reference(&format!("refs/heads/{branch_name}"))
                .map_err(git_message)?;
            let mut opts = git2::WorktreeAddOptions::new();
            opts.reference(Some(&reference));
            repo.worktree(&chat_id, &wt_path, Some(&opts))
                .map_err(git_message)?;
            // git_worktree_add checks the branch out as part of creation; a
            // missing checkout would starve every run's tools.
            let populated = std::fs::read_dir(&wt_path)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .any(|entry| entry.file_name() != ".git")
                })
                .unwrap_or(false);
            if !populated {
                return Err(format!(
                    "session worktree {} was created without a checkout",
                    wt_path.display()
                ));
            }
            Ok(wt_path.display().to_string())
        })
        .await
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

/// A registered worktree's path, trailing slash stripped (libgit2 spells
/// worktree paths with one).
fn registered_worktree_path(main_repo: &Repository, worktree: &str) -> Result<PathBuf, String> {
    let path = main_repo
        .find_worktree(worktree)
        .map_err(|_| format!("worktree {worktree} is not registered"))?
        .path()
        .to_path_buf();
    Ok(PathBuf::from(
        path.display().to_string().trim_end_matches('/'),
    ))
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

/// The refusal the UI's switch dialog parses (ADR-branch-is-live-working-tree-state — inform-only, never
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

fn git_message(error: git2::Error) -> String {
    error.message().to_string()
}

/// A [`MergeStop`] as the git layer's error string: refusals carry the
/// stable `merge-refused:` first line plus one detail per line, so the RPC
/// boundary and the UI classify without parsing anything else.
fn merge_stop_to_string(stop: MergeStop) -> String {
    match stop {
        MergeStop::Refusal(reason, detail) => merge_refusal_message(reason, &detail),
        MergeStop::Failed(message) => message,
    }
}

/// One status entry's concise classification for the File sidebar's
/// markers (ticket 10). `None` for entries that carry no working-tree
/// signal (clean files). Conflicted paths report `Conflicted` (ADR-ui-driven-git-writes-agent-surface-stays-read-only):
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

/// The porcelain sides of one status entry (ADR-ui-driven-git-writes-agent-surface-stays-read-only): the index side is
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

fn same_path(left: &Path, right: &Path) -> bool {
    let normalize_existing =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path));
    normalize_existing(left) == normalize_existing(right)
}

// ---- checkout identity ----

/// Canonical checkout identity (ADR-git-hangs-off-spaces-no-repo-registry): `sha256(deviceId ‖ NUL ‖ git_dir)`,
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

/// Hex SHA-256 of raw bytes — the Turn records' content-hash formula.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
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

// ---- turn baseline (ADR-turn-diff-net-change-baseline) ----

/// The net-change starting point of a chat's latest Turn, captured
/// synchronously when a queued command is accepted: the HEAD sha and the
/// uncommitted patch at turn start. In-memory and latest-per-chat; an
/// engine restart drops them.
#[derive(Debug, Clone)]
pub(crate) struct TurnBaseline {
    pub head_sha: String,
    pub patch: String,
}

/// One Turn's captured net change (ADR-turn-change-sets-and-review): the typed files plus their
/// totals, ready for the `TurnChangeSet` wire shape.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnChangeCapture {
    pub files: Vec<TurnFileChange>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
}

/// The settle-time freeze of one Turn's change set (ADR-turn-change-sets-and-review ticket 02):
/// the summary and its immutable per-file content, captured from one
/// working-tree snapshot in one locked Git pass.
#[derive(Debug, Clone)]
pub(crate) struct TurnChangeFreeze {
    pub capture: TurnChangeCapture,
    pub content: Vec<TurnFileContent>,
}

/// One file's immutable before/after pair inside a settled Turn's persisted
/// change-set record (ADR-turn-change-sets-and-review ticket 02): the turn-start content on the old
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

/// The net-change filter (ADR-turn-diff-net-change-baseline): keep only files whose CURRENT
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

/// The "Latest turn" capture (ADR-turn-diff-net-change-baseline): diff `HEAD@start → workdir with
/// index`, then keep only the files whose current section differs from the
/// baseline section — the net change since the Turn began.
/// The uncommitted change set as section texts: path → patch section,
/// untracked included — the same shape [`filter_turn_patch`] compares, so
/// a bash command's before/after maps differ exactly where the command
/// (or anyone) wrote during its window.
fn workdir_sections(repo: &Repository) -> Result<HashMap<String, String>, GitFault> {
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
        .map_err(git_message)
        .map_err(GitFault::Error)?;
    let text = diff_patch_text(&mut diff);
    Ok(split_sections(&text)
        .into_iter()
        .map(|section| (section.path, section.text))
        .collect())
}

/// What a call a crash cut off changed since its start
/// (ADR-step-checkpoints-in-a-chat-owned-object-store). Blocking, outside
/// the per-checkout lock: it runs once, when a chat loads after a restart,
/// before any of that chat's work. `None` when `repo_path` is no longer a
/// work tree.
pub(crate) fn files_since(
    repo_path: &str,
    store: &Path,
    start: &CallStart,
) -> Result<Option<Vec<CheckpointFile>>, String> {
    let repo = Repository::discover(repo_path).map_err(git_message)?;
    let sections = workdir_sections(&repo).map_err(GitFault::into_string)?;
    checkpoints::files_since(&repo, store, start, &sections).map_err(git_message)
}

/// A checkpoint blob's bytes; `None` when the store or the blob is gone.
pub(crate) fn checkpoint_blob(store: &Path, id: &str) -> Option<Vec<u8>> {
    let store = Repository::open_bare(store).ok()?;
    let blob = store.find_blob(git2::Oid::from_str(id).ok()?).ok()?;
    Some(blob.content().to_vec())
}

/// Copy the blobs `ids` names from one checkpoint store into a new one at
/// `to` — the repack that reclaims what no step references any more. A
/// blob already gone is skipped.
pub(crate) fn copy_checkpoint_blobs(
    from: &Path,
    to: &Path,
    ids: &HashSet<String>,
) -> Result<(), String> {
    let from = Repository::open_bare(from).map_err(git_message)?;
    let to = Repository::init_bare(to).map_err(git_message)?;
    for id in ids {
        let Ok(oid) = git2::Oid::from_str(id) else {
            continue;
        };
        if let Ok(blob) = from.find_blob(oid) {
            to.blob(blob.content()).map_err(git_message)?;
        }
    }
    Ok(())
}

/// Store `bytes` as a checkpoint blob — a test's way to fill a store.
#[cfg(test)]
pub(crate) fn put_checkpoint_blob(store: &Path, bytes: &[u8]) -> String {
    checkpoints::open_store(store)
        .unwrap()
        .blob(bytes)
        .unwrap()
        .to_string()
}

/// `path` (absolute, or relative to `repo_root`) as a repo-root-relative
/// path; `None` when it escapes the work tree.
fn relative_path(repo: &Repository, repo_root: &str, path: &str) -> Option<String> {
    let workdir = repo.workdir()?;
    let candidate = Path::new(path);
    let absolute = if candidate.is_absolute() {
        normalize_path(candidate)
    } else {
        normalize_path(&Path::new(repo_root).join(candidate))
    };
    // The workdir git reports is a realpath (macOS /var → /private/var
    // and every other symlink below tmp), while the tool's path rides
    // the chat's cwd spelling. Resolve through the symlinks — the
    // target may not exist yet (a write tool creating it), so take
    // the deepest existing ancestor and re-attach the tail. Both sides
    // drop Windows's verbatim (`\\?\`) prefix first — canonicalize
    // produces it, git2's workdir never carries it, and a prefix
    // mismatch silently refuses the strip.
    let resolved = crate::tools::strip_verbatim(resolve_existing(&absolute)?);
    resolved
        .strip_prefix(crate::tools::strip_verbatim(workdir.to_path_buf()))
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

/// Lexical normalization — resolve `.` and `..` without touching the
/// filesystem: the target may not exist yet (a write tool creating it).
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The symlink-resolved absolute form of `path`: the deepest EXISTING
/// ancestor is canonicalized, the not-yet-existing tail re-attached. The
/// write tool's target usually does not exist yet when attribution runs.
fn resolve_existing(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(resolved) => {
                let mut out = resolved;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return Some(out);
            }
            Err(_) => {
                let name = current.file_name()?.to_os_string();
                tail.push(name);
                if !current.pop() {
                    return None;
                }
            }
        }
    }
}

fn turn_capture(
    repo: &Repository,
    device_id: &str,
    baseline: &TurnBaseline,
    attributed: Option<&HashSet<String>>,
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
    let (net_patch, kept) = filter_turn_patch(&baseline.patch, &current_patch);
    files.retain(|file| kept.contains(&file.path));
    // The attribution filter (the second of the two orthogonal filters):
    // files outside the Turn's own write set are another writer's — a
    // concurrent chat in the same working tree — and drop out here, ahead
    // of the totals, so every consumer (card, review, patch, checksum)
    // sees the same narrowed set.
    if let Some(attributed) = attributed {
        files.retain(|file| attributed.contains(&file.path));
    }
    // The patch text follows the surviving files: the net patch without the
    // sections attribution dropped. Section keys are the new-side path, the
    // same key the file summaries use. Without an attribution set the two
    // filters coincide — the kept paths are the net filter's own.
    let keep: HashSet<String> = match attributed {
        Some(set) => set.clone(),
        None => kept.iter().cloned().collect(),
    };
    let patch: String = split_sections(&net_patch)
        .into_iter()
        .filter(|section| keep.contains(&section.path))
        .map(|section| section.text)
        .collect();
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

/// Ref labels by target commit, resolved once for a whole walk.
fn refs_by_commit(
    repo: &Repository,
) -> Result<std::collections::HashMap<git2::Oid, Vec<holt_proto::GitHistoryRef>>, String> {
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
    Ok(refs_by_commit)
}

/// One history row's model: identity, topology, author, and refs.
fn history_row(
    commit: &git2::Commit<'_>,
    refs_by_commit: &std::collections::HashMap<git2::Oid, Vec<holt_proto::GitHistoryRef>>,
) -> Result<holt_proto::GitHistoryCommit, String> {
    let parent_shas: Vec<String> = (0..commit.parent_count())
        .filter_map(|index| commit.parent_id(index).ok().map(|oid| oid.to_string()))
        .collect();
    let author = commit.author();
    let authored_at = chrono::DateTime::from_timestamp(author.when().seconds(), 0)
        .map(|time| time.to_rfc3339())
        .unwrap_or_default();
    Ok(holt_proto::GitHistoryCommit {
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
    })
}

/// Up to `limit` commits from `head`, topologically, tracking whether the
/// walk had more to give. Shared by the history page and the integration
/// snapshot's trunk and per-worktree ranges.
fn walk_commits(
    repo: &Repository,
    head: git2::Oid,
    hide: Option<git2::Oid>,
    limit: usize,
    refs_by_commit: &std::collections::HashMap<git2::Oid, Vec<holt_proto::GitHistoryRef>>,
) -> Result<(Vec<holt_proto::GitHistoryCommit>, bool), String> {
    let mut walker = repo.revwalk().map_err(git_message)?;
    walker
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .map_err(git_message)?;
    walker.push(head).map_err(git_message)?;
    if let Some(hidden) = hide {
        walker.hide(hidden).map_err(git_message)?;
    }
    let mut commits = Vec::new();
    let mut truncated = false;
    for oid in walker {
        let oid = oid.map_err(git_message)?;
        if commits.len() >= limit {
            truncated = true;
            break;
        }
        let commit = repo.find_commit(oid).map_err(git_message)?;
        commits.push(history_row(&commit, refs_by_commit)?);
    }
    Ok((commits, truncated))
}

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

    let refs = refs_by_commit(repo)?;

    // The page collects at most `limit` commits after the cursor, but the
    // walk runs to the end: the totals need the full count (oid walking is
    // cheap; only the page's commits get loaded).
    let mut walker = repo.revwalk().map_err(git_message)?;
    walker
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .map_err(git_message)?;
    walker.push_head().map_err(git_message)?;
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
        commits.push(history_row(&commit, &refs)?);
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

/// How many trunk commits the integration snapshot carries, and the same
/// cap for one worktree's `trunk..head` range.
const INTEGRATION_COMMIT_CAP: usize = 200;

/// How deep the snapshot hunts a worktree's `Holt-Merged-From` merge point
/// down the trunk's first-parent line. Deeper than the commit window — a
/// squash older than 200 trunk commits is still that worktree's merge point,
/// and the branch-head fallback is only right for a fast-forward.
const INTEGRATION_TRAILER_SEARCH_CAP: usize = 5_000;

/// The trunk commit carrying `trailer` on the first-parent line, if the
/// trunk still has it.
fn trunk_trailer_commit(
    main_repo: &Repository,
    trunk_oid: git2::Oid,
    trailer: &str,
) -> Option<String> {
    let mut walker = main_repo.revwalk().ok()?;
    let _ = walker.simplify_first_parent();
    walker.push(trunk_oid).ok()?;
    for oid in walker
        .by_ref()
        .take(INTEGRATION_TRAILER_SEARCH_CAP)
        .flatten()
    {
        let message = main_repo
            .find_commit(oid)
            .ok()
            .and_then(|commit| commit.message().ok().map(str::to_string));
        if message.is_some_and(|message| message.lines().any(|line| line.trim() == trailer)) {
            return Some(oid.to_string());
        }
    }
    None
}

/// The `Holt-Merged-From` trailer line a squash merge writes (and the
/// integration view reads back to find the merge point).
fn merged_from_trailer(worktree: &str) -> String {
    format!("Holt-Merged-From: {worktree}")
}

/// The read-only integration snapshot (the `ListIntegration` reply's git
/// side): the trunk's commit line plus every linked worktree's position
/// relative to it. One pass under the checkout lock; a worktree that cannot
/// be read reports as `missing` instead of failing the whole snapshot.
fn integration_snapshot(
    repo: &Repository,
    trunk_override: Option<&str>,
    checks_root: &Path,
) -> Result<holt_proto::IntegrationSnapshot, String> {
    // The discovered repository may be a linked worktree; worktree listing
    // and refs are common-dir-backed, but the main checkout's path and HEAD
    // come from the main repository itself.
    let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
    let names = local_branch_names(&main_repo)?;
    let trunk_name = match trunk_override {
        Some(name) => {
            if !names.iter().any(|candidate| candidate == name) {
                return Err(format!("trunk {name} does not resolve"));
            }
            name.to_string()
        }
        None => default_branch(&names, origin_head_target(&main_repo).as_deref())
            .ok_or_else(|| "no branches to pick a trunk from".to_string())?
            .to_string(),
    };
    let trunk_commit = main_repo
        .find_reference(&format!("refs/heads/{trunk_name}"))
        .map_err(git_message)?
        .peel_to_commit()
        .map_err(|_| format!("trunk {trunk_name} does not resolve"))?;
    let trunk_oid = trunk_commit.id();

    let refs = refs_by_commit(&main_repo)?;
    let (trunk_commits, trunk_truncated) =
        walk_commits(&main_repo, trunk_oid, None, INTEGRATION_COMMIT_CAP, &refs)?;

    let main_checkout = integration_main_checkout(&main_repo)?;

    // The engine's own root and a registered worktree's path can spell the
    // same directory differently on Windows (a short %TEMP% prefix on one
    // side, the expanded form libgit2 stored on the other), so the checks
    // classification below runs against the canonicalized spelling too.
    let canonical_checks_root = std::fs::canonicalize(checks_root)
        .ok()
        .map(crate::tools::strip_verbatim);

    let mut worktrees = Vec::new();
    let registered = main_repo.worktrees().map_err(git_message)?;
    for index in 0..registered.len() {
        let Ok(Some(name)) = registered.get(index) else {
            continue;
        };
        let name = name.to_string();
        let Ok(worktree) = main_repo.find_worktree(&name) else {
            continue;
        };
        let path = worktree.path().to_path_buf();
        // A checks worktree (under the engine's throwaway root) never
        // belongs in the view; a crashed run leaves its registration
        // behind with the directory gone — prune it while we are here. A
        // LIVE run's checkout (directory present) is left alone: pruning
        // under a running check would break it.
        let under_checks_root = path.starts_with(checks_root)
            || canonical_checks_root
                .as_deref()
                .is_some_and(|root| path.starts_with(root));
        if under_checks_root {
            if !path.is_dir() {
                let mut prune = git2::WorktreePruneOptions::new();
                prune.valid(true).locked(true).working_tree(true);
                let _ = worktree.prune(Some(&mut prune));
                // A crash between the worktree add and the branch delete
                // also strands the temp branch — take it with us.
                let _ = main_repo
                    .find_reference(&format!("refs/heads/holt/check/{name}"))
                    .map(|mut reference| reference.delete());
            }
            continue;
        }
        let mut row = holt_proto::IntegrationWorktree {
            name,
            path: path.display().to_string(),
            branch: None,
            head_sha: None,
            fork_sha: None,
            ahead: 0,
            behind: 0,
            dirty: false,
            dirty_files: 0,
            merged: false,
            merge_sha: None,
            missing: !path.is_dir(),
            chat_id: None,
            commits: Vec::new(),
            truncated: false,
        };
        if !row.missing {
            integration_fill_worktree(&mut row, &path, trunk_oid, &refs)?;
        }
        if row.merged {
            // The squash merge point is the trunk commit carrying this
            // worktree's trailer; a fast-forward merge writes none, and the
            // branch head is the merge point itself.
            let trailer = merged_from_trailer(&row.name);
            row.merge_sha = trunk_trailer_commit(&main_repo, trunk_oid, &trailer)
                .or_else(|| row.head_sha.clone());
        }
        worktrees.push(row);
    }
    worktrees.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(holt_proto::IntegrationSnapshot {
        repo_path: main_repo
            .workdir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| main_repo.path().display().to_string()),
        trunk: holt_proto::IntegrationTrunk {
            name: trunk_name,
            head_sha: trunk_oid.to_string(),
        },
        trunk_commits,
        trunk_truncated,
        main_checkout,
        worktrees,
    })
}

/// The main checkout row: path, checked-out branch (empty for detached),
/// and whether it holds uncommitted changes (untracked included, ignored
/// excluded).
fn integration_main_checkout(
    main_repo: &Repository,
) -> Result<holt_proto::IntegrationMainCheckout, String> {
    let branch = current_branch(main_repo);
    let dirty = if main_repo.workdir().is_some() {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        let statuses = main_repo
            .statuses(Some(&mut options))
            .map_err(git_message)?;
        !statuses.is_empty()
    } else {
        false
    };
    Ok(holt_proto::IntegrationMainCheckout {
        path: main_repo
            .workdir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
        branch,
        dirty,
    })
}

/// Fill one linked worktree's git facts from its own repository handle. A
/// directory that opens but cannot be read — a corrupt index, an unreadable
/// object store — counts as missing too: the snapshot stays whole, one
/// worktree's failure never fails the call.
fn integration_fill_worktree(
    row: &mut holt_proto::IntegrationWorktree,
    path: &Path,
    trunk_oid: git2::Oid,
    refs: &std::collections::HashMap<git2::Oid, Vec<holt_proto::GitHistoryRef>>,
) -> Result<(), String> {
    let Ok(worktree_repo) = Repository::open(path) else {
        row.missing = true;
        return Ok(());
    };
    let mut read = || -> Result<(), String> {
        if let Ok(head) = worktree_repo.head() {
            if head.is_branch()
                && let Ok(shorthand) = head.shorthand()
                && !shorthand.is_empty()
            {
                row.branch = Some(shorthand.to_string());
            }
            if let Ok(head_commit) = head.peel_to_commit() {
                let head_oid = head_commit.id();
                row.head_sha = Some(head_oid.to_string());
                row.fork_sha = worktree_repo
                    .merge_base(trunk_oid, head_oid)
                    .ok()
                    .map(|oid| oid.to_string());
                if let Ok((ahead, behind)) = worktree_repo.graph_ahead_behind(head_oid, trunk_oid) {
                    row.ahead = ahead;
                    row.behind = behind;
                }
                let (commits, truncated) = walk_commits(
                    &worktree_repo,
                    head_oid,
                    Some(trunk_oid),
                    INTEGRATION_COMMIT_CAP,
                    refs,
                )?;
                row.commits = commits;
                row.truncated = truncated;
                // Merged means Holt's own marker ref still points at this
                // head — ancestry cannot say it after a squash.
                row.merged = worktree_repo
                    .find_reference(&format!("refs/holt/merged/{}", row.name))
                    .and_then(|reference| reference.peel_to_commit())
                    .is_ok_and(|commit| commit.id() == head_oid);
            }
        }
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        let statuses = worktree_repo
            .statuses(Some(&mut options))
            .map_err(git_message)?;
        row.dirty = !statuses.is_empty();
        row.dirty_files = statuses.len();
        Ok(())
    };
    if read().is_err() {
        // Unreadable: report missing with the identity fields intact and
        // nothing half-filled.
        row.branch = None;
        row.head_sha = None;
        row.fork_sha = None;
        row.ahead = 0;
        row.behind = 0;
        row.dirty = false;
        row.dirty_files = 0;
        row.merged = false;
        row.merge_sha = None;
        row.commits.clear();
        row.truncated = false;
        row.missing = true;
    }
    Ok(())
}

// ---- local merge (the integration view's merge bar) ----

/// The refusal message's stable first line. Deliberately NOT
/// [`SWITCH_REFUSAL_MARKER`]: the branch-switch dialog recognizes its own
/// marker, and a merge refusal must never raise that dialog.
const MERGE_REFUSAL_MARKER: &str = "merge-refused: ";

/// How a merge stopped: a typed refusal the UI can act on, or an opaque
/// git/engine failure.
enum MergeStop {
    Refusal(holt_proto::MergeRefusal, Vec<String>),
    Failed(String),
}

fn merge_refusal_message(reason: holt_proto::MergeRefusal, detail: &[String]) -> String {
    let mut message = format!("{MERGE_REFUSAL_MARKER}{}", reason.wire_name());
    for line in detail {
        message.push('\n');
        message.push_str(line);
    }
    message
}

/// Where the trunk is checked out — decides whether a landed merge syncs a
/// working tree (main checkout), refuses outright (a linked worktree holds
/// it: moving the ref there would strand that worktree's index and files),
/// or only moves the ref.
enum TrunkHeldBy {
    MainCheckout,
    LinkedWorktree(PathBuf),
    Nobody,
}

/// The facts both Preview and Merge establish under the checkout lock.
struct MergeContext {
    branch: String,
    branch_head: git2::Oid,
    trunk_ref: String,
    trunk_head: git2::Oid,
    trunk_held_by: TrunkHeldBy,
}

/// The shared pre-checks: resolve the worktree's branch and the trunk,
/// refuse the empty and impossible merges, and locate the trunk's checkout.
fn merge_context(
    main_repo: &Repository,
    worktree_name: &str,
    trunk: &str,
    mode: &str,
) -> Result<MergeContext, MergeStop> {
    let worktree = main_repo.find_worktree(worktree_name).map_err(|_| {
        MergeStop::Refusal(
            holt_proto::MergeRefusal::WorktreeMissing,
            vec![worktree_name.to_string()],
        )
    })?;
    let path = worktree.path().to_path_buf();
    if !path.is_dir() {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::WorktreeMissing,
            vec![path.display().to_string()],
        ));
    }
    let worktree_repo = Repository::open(&path).map_err(|_| {
        MergeStop::Refusal(
            holt_proto::MergeRefusal::WorktreeMissing,
            vec![path.display().to_string()],
        )
    })?;
    let head = worktree_repo
        .head()
        .map_err(|_| MergeStop::Refusal(holt_proto::MergeRefusal::BranchDetached, Vec::new()))?;
    if !head.is_branch() {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::BranchDetached,
            Vec::new(),
        ));
    }
    let branch = head
        .shorthand()
        .map_err(git_message)
        .map_err(MergeStop::Failed)?
        .to_string();
    let branch_head = head
        .peel_to_commit()
        .map_err(|_| MergeStop::Refusal(holt_proto::MergeRefusal::BranchDetached, Vec::new()))?
        .id();

    let trunk_ref = format!("refs/heads/{trunk}");
    let trunk_head = main_repo
        .find_reference(&trunk_ref)
        .and_then(|reference| reference.peel_to_commit())
        .map_err(|_| MergeStop::Failed(format!("trunk {trunk} does not resolve")))?
        .id();

    let (ahead, behind) = main_repo
        .graph_ahead_behind(branch_head, trunk_head)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    if ahead == 0 {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::NothingToMerge,
            Vec::new(),
        ));
    }
    if mode == "ff" && behind > 0 {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::FfNotPossible,
            Vec::new(),
        ));
    }

    let trunk_held_by = if current_branch(main_repo).as_deref() == Some(trunk) {
        TrunkHeldBy::MainCheckout
    } else if let Some((_, path)) = linked_worktree_heads(main_repo)
        .into_iter()
        .find(|(head_branch, _)| head_branch == trunk)
    {
        // libgit2 spells worktree paths with a trailing slash; the refusal
        // detail is a user-facing path.
        TrunkHeldBy::LinkedWorktree(PathBuf::from(
            path.display().to_string().trim_end_matches('/'),
        ))
    } else {
        TrunkHeldBy::Nobody
    };
    Ok(MergeContext {
        branch,
        branch_head,
        trunk_ref,
        trunk_head,
        trunk_held_by,
    })
}

fn commit_tree(repo: &Repository, oid: git2::Oid) -> Result<git2::Tree<'_>, MergeStop> {
    repo.find_commit(oid)
        .and_then(|commit| commit.tree())
        .map_err(git_message)
        .map_err(MergeStop::Failed)
}

/// The merged tree, in memory. Squash three-ways the trees (no ancestry
/// after the merge: the commit's only parent is the trunk head); ff adopts
/// the branch tree wholesale. Conflicts come back as paths — Holt leaves no
/// conflict state anywhere on disk.
fn merged_tree(
    main_repo: &Repository,
    context: &MergeContext,
    mode: &str,
) -> Result<Result<git2::Oid, Vec<String>>, MergeStop> {
    if mode == "ff" {
        return commit_tree(main_repo, context.branch_head).map(|tree| Ok(tree.id()));
    }
    let ancestor = main_repo
        .merge_base(context.trunk_head, context.branch_head)
        .and_then(|base| main_repo.find_commit(base).and_then(|commit| commit.tree()))
        // Unrelated histories merge against the empty tree, like
        // `git merge --allow-unrelated-histories`.
        .or_else(|_| {
            let tree = main_repo.treebuilder(None)?.write()?;
            main_repo.find_tree(tree)
        })
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    let ours = commit_tree(main_repo, context.trunk_head)?;
    let theirs = commit_tree(main_repo, context.branch_head)?;
    let mut index = main_repo
        .merge_trees(&ancestor, &ours, &theirs, None)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    if index.has_conflicts() {
        let mut paths: Vec<String> = index
            .conflicts()
            .map_err(git_message)
            .map_err(MergeStop::Failed)?
            .filter_map(|conflict| match conflict {
                Ok(conflict) => conflict
                    .our
                    .or(conflict.their)
                    .or(conflict.ancestor)
                    .map(|entry| String::from_utf8_lossy(&entry.path).into_owned()),
                Err(_) => None,
            })
            .collect();
        paths.sort();
        paths.dedup();
        return Ok(Err(paths));
    }
    let tree = index
        .write_tree_to(main_repo)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    Ok(Ok(tree))
}

/// `tree(trunk commit) → tree` as the preview payload, under the shared
/// caps. Both ends arrive as commit oids and peel here.
fn merge_preview_diff(
    main_repo: &Repository,
    from_commit: git2::Oid,
    to_tree: git2::Oid,
) -> Result<(Vec<DiffFileSummary>, String, bool, u32, u32), MergeStop> {
    let from = commit_tree(main_repo, from_commit)?;
    let to = main_repo
        .find_tree(to_tree)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    let mut options = git2::DiffOptions::new();
    options.show_binary(true);
    let mut diff = main_repo
        .diff_tree_to_tree(Some(&from), Some(&to), Some(&mut options))
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    let _ = diff.find_similar(Some(&mut find));
    let files = diff_summaries(&mut diff);
    let patch = diff_patch_text(&mut diff);
    let (patch, truncated) = truncate_patch(&patch, MAX_PATCH_BYTES);
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    Ok((files, patch, truncated, additions, deletions))
}

/// The squash mode's default commit message: the chat's title (or the
/// branch's oldest subject), the squashed subjects, and the trailer the
/// merge writes and `ListIntegration` reads back.
fn squash_message(
    main_repo: &Repository,
    context: &MergeContext,
    worktree_name: &str,
    title: Option<&str>,
) -> Result<String, MergeStop> {
    let mut subjects: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut walker = main_repo
        .revwalk()
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    walker
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    walker
        .push(context.branch_head)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    walker
        .hide(context.trunk_head)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    for oid in walker {
        let oid = oid.map_err(git_message).map_err(MergeStop::Failed)?;
        total += 1;
        if subjects.len() < INTEGRATION_COMMIT_CAP {
            let commit = main_repo
                .find_commit(oid)
                .map_err(git_message)
                .map_err(MergeStop::Failed)?;
            subjects.push(
                commit
                    .summary()
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
    }
    let title = match title {
        Some(title) => title.to_string(),
        // The branch's oldest commit — subjects are newest-first.
        None => subjects
            .last()
            .cloned()
            .filter(|subject| !subject.is_empty())
            .unwrap_or_else(|| format!("Merge {}", context.branch)),
    };
    let mut message = format!(
        "{title}\n\nSquashed {} commits from {}:\n",
        total, context.branch
    );
    for subject in &subjects {
        message.push_str("- ");
        message.push_str(subject);
        message.push('\n');
    }
    if total > subjects.len() {
        message.push_str(&format!("- … ({} more)\n", total - subjects.len()));
    }
    message.push('\n');
    message.push_str(&merged_from_trailer(worktree_name));
    Ok(message)
}

/// The whole preview: pre-checks, the in-memory tree, and its diff — never
/// a working-tree write. Pre-check refusals arrive as `outcome.refused`
/// (the wire contract reserves `Failed` for the merge call); only hard git
/// failures error the RPC.
fn merge_preview(
    repo: &Repository,
    worktree_name: &str,
    trunk: &str,
    mode: &str,
    title: Option<&str>,
) -> Result<holt_proto::MergePreview, String> {
    match merge_preview_inner(repo, worktree_name, trunk, mode, title) {
        Ok(preview) => Ok(preview),
        Err(MergeStop::Refusal(reason, detail)) => Ok(holt_proto::MergePreview {
            heads: holt_proto::MergeHeads {
                trunk_head: String::new(),
                branch_head: String::new(),
            },
            outcome: holt_proto::MergeOutcome::Refused { reason, detail },
        }),
        Err(MergeStop::Failed(message)) => Err(message),
    }
}

fn merge_preview_inner(
    repo: &Repository,
    worktree_name: &str,
    trunk: &str,
    mode: &str,
    title: Option<&str>,
) -> Result<holt_proto::MergePreview, MergeStop> {
    let main_repo = Repository::open(repo.commondir())
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    let context = merge_context(&main_repo, worktree_name, trunk, mode)?;
    if let TrunkHeldBy::LinkedWorktree(path) = &context.trunk_held_by {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::TrunkCheckedOutElsewhere,
            vec![path.display().to_string()],
        ));
    }
    let heads = holt_proto::MergeHeads {
        trunk_head: context.trunk_head.to_string(),
        branch_head: context.branch_head.to_string(),
    };
    let tree = match merged_tree(&main_repo, &context, mode)? {
        Ok(tree) => tree,
        Err(paths) => {
            return Ok(holt_proto::MergePreview {
                heads,
                outcome: holt_proto::MergeOutcome::Conflict { paths },
            });
        }
    };
    let (files, patch, truncated, additions, deletions) =
        merge_preview_diff(&main_repo, context.trunk_head, tree)?;
    let message = if mode == "ff" {
        String::new()
    } else {
        squash_message(&main_repo, &context, worktree_name, title)?
    };
    Ok(holt_proto::MergePreview {
        heads,
        outcome: holt_proto::MergeOutcome::Ok {
            files,
            patch,
            truncated,
            message,
            additions,
            deletions,
        },
    })
}

/// Land the merge. Everything from the heads check to the last ref write
/// runs under the caller's checkout lock; a failure after the working-tree
/// sync rolls the tree back before answering, so no path leaves a
/// half-merged checkout.
#[allow(clippy::too_many_arguments)]
fn merge_into_trunk(
    repo: &Repository,
    worktree_name: &str,
    trunk: &str,
    mode: &str,
    message: Option<String>,
    expect: &holt_proto::MergeHeads,
) -> Result<holt_proto::MergeResult, MergeStop> {
    let main_repo = Repository::open(repo.commondir())
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    let context = merge_context(&main_repo, worktree_name, trunk, mode)?;
    if let TrunkHeldBy::LinkedWorktree(path) = &context.trunk_held_by {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::TrunkCheckedOutElsewhere,
            vec![path.display().to_string()],
        ));
    }
    let expect_trunk = git2::Oid::from_str(&expect.trunk_head)
        .map_err(|_| MergeStop::Failed("expected trunkHead is not a sha".into()))?;
    let expect_branch = git2::Oid::from_str(&expect.branch_head)
        .map_err(|_| MergeStop::Failed("expected branchHead is not a sha".into()))?;
    if context.trunk_head != expect_trunk || context.branch_head != expect_branch {
        return Err(MergeStop::Refusal(
            holt_proto::MergeRefusal::Stale,
            Vec::new(),
        ));
    }
    let tree = merged_tree(&main_repo, &context, mode)?.map_err(|paths| {
        MergeStop::Failed(format!(
            "the merge conflicts ({}); re-run the preview",
            paths.join(", ")
        ))
    })?;

    // The squash commit lands in the object store only — no ref moves it.
    // ff needs no new object at all.
    let merged_oid = if mode == "ff" {
        context.branch_head
    } else {
        let mut message = message.unwrap_or_else(|| {
            squash_message(&main_repo, &context, worktree_name, None).unwrap_or_default()
        });
        let trailer = merged_from_trailer(worktree_name);
        if !message.lines().any(|line| line.trim() == trailer) {
            if !message.ends_with('\n') {
                message.push('\n');
            }
            message.push('\n');
            message.push_str(&trailer);
        }
        let signature = main_repo.signature().map_err(|_| {
            MergeStop::Failed(
                "could not read the git identity (user.name/user.email); \
                 configure it before merging"
                    .into(),
            )
        })?;
        let tree_object = main_repo
            .find_tree(tree)
            .map_err(git_message)
            .map_err(MergeStop::Failed)?;
        let parent = main_repo
            .find_commit(context.trunk_head)
            .map_err(git_message)
            .map_err(MergeStop::Failed)?;
        main_repo
            .commit(
                None,
                &signature,
                &signature,
                &message,
                &tree_object,
                &[&parent],
            )
            .map_err(git_message)
            .map_err(MergeStop::Failed)?
    };

    // Sync the main checkout's working tree BEFORE the ref moves: a safe
    // checkout uses HEAD's tree as its baseline, which must still be the
    // old trunk when it runs. Uncommitted changes outside the merge's
    // files survive — that is the safe checkout's own contract.
    let synced = matches!(context.trunk_held_by, TrunkHeldBy::MainCheckout);
    if synced {
        let tree_object = main_repo
            .find_tree(tree)
            .map_err(git_message)
            .map_err(MergeStop::Failed)?;
        let conflicts = switch_conflicts(&main_repo, &tree_object).map_err(MergeStop::Failed)?;
        if !conflicts.is_empty() {
            return Err(MergeStop::Refusal(
                holt_proto::MergeRefusal::TrunkCheckedOutDirty,
                conflicts,
            ));
        }
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.safe();
        main_repo
            .checkout_tree(tree_object.as_object(), Some(&mut checkout))
            .map_err(git_message)
            .map_err(MergeStop::Failed)?;
    }

    // Move the ref (the main checkout's HEAD is a symbolic ref onto it and
    // follows). A failure here rolls the working tree back to the old
    // trunk before answering — the ref never moved.
    let reflog = format!("holt: merge {worktree_name} ({mode})");
    let mut trunk_reference = main_repo
        .find_reference(&context.trunk_ref)
        .map_err(git_message)
        .map_err(MergeStop::Failed)?;
    if let Err(set_target_error) = trunk_reference.set_target(merged_oid, &reflog) {
        if synced {
            // HEAD never moved, so a safe checkout of the old tree is a
            // measured no-op (libgit2 diffs the target against HEAD). Undo
            // the merge's OWN paths by force-checking out HEAD over exactly
            // them — files the merge added disappear, files it changed go
            // back, and unrelated uncommitted changes sit outside the
            // pathspec and survive.
            let old_tree = commit_tree(&main_repo, context.trunk_head)?;
            let new_tree = main_repo
                .find_tree(tree)
                .map_err(git_message)
                .map_err(MergeStop::Failed)?;
            let delta_paths: Vec<String> = main_repo
                .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
                .map_err(git_message)
                .map_err(MergeStop::Failed)?
                .deltas()
                .filter_map(|delta| {
                    delta
                        .new_file()
                        .path()
                        .or_else(|| delta.old_file().path())
                        .map(|path| path.display().to_string())
                })
                .collect();
            let mut rollback = git2::build::CheckoutBuilder::new();
            rollback.force();
            for path in &delta_paths {
                rollback.path(path.as_str());
            }
            if let Err(error) = main_repo.checkout_head(Some(&mut rollback)) {
                return Err(MergeStop::Failed(format!(
                    "the merge failed ({}) and rolling the checkout back failed too: {error}",
                    git_message(set_target_error)
                )));
            }
        }
        return Err(MergeStop::Failed(git_message(set_target_error)));
    }

    // Holt's own merged marker — the one thing that says "merged" after a
    // squash (the branch head is not in the trunk's history).
    let _ = main_repo.reference(
        &format!("refs/holt/merged/{worktree_name}"),
        context.branch_head,
        true,
        &reflog,
    );

    Ok(holt_proto::MergeResult {
        merged_sha: merged_oid.to_string(),
        main_checkout_synced: synced,
    })
}

/// The squash-merged tree a checks run executes against, with the heads it
/// was computed from (the staleness pair).
pub(crate) struct ChecksMerge {
    pub(crate) tree: String,
    pub(crate) trunk_head: String,
    pub(crate) branch_head: String,
}

/// What [`remove_worktree`] took away, for the caller's bookkeeping.
pub(crate) struct RemovedWorktree {
    pub path: String,
}

/// The post-branch half of the checks checkout: create the parent and the
/// worktree, detach its HEAD, drop the temp branch. The caller deletes the
/// branch when this fails.
fn checkout_detached_tree_finish(
    main_repo: &Repository,
    name: &str,
    branch: &str,
    commit: git2::Oid,
    target: &Path,
) -> Result<(), String> {
    // git_worktree_add does not create intermediate directories.
    if let Some(parent_dir) = target.parent() {
        std::fs::create_dir_all(parent_dir)
            .map_err(|error| format!("could not create {}: {error}", parent_dir.display()))?;
    }
    if target.exists() {
        return Err(format!(
            "the checkout directory already exists: {}",
            target.display()
        ));
    }
    let reference = main_repo.find_reference(branch).map_err(git_message)?;
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&reference));
    let added = main_repo.worktree(name, target, Some(&options));
    drop(reference);
    let worktree = added.map_err(|error| {
        // The add failed before anything landed: take the directory back
        // if a stub survived.
        if target.exists() {
            let _ = std::fs::remove_dir_all(target);
        }
        format!("could not create the checks worktree: {error}")
    })?;
    // Detach the worktree's HEAD: the checkout is content-addressed by its
    // commit, which the worktree remembers. A failure here removes the
    // worktree entirely — the branch is the caller's to drop.
    let detached = Repository::open(target)
        .map_err(git_message)
        .and_then(|worktree_repo| worktree_repo.set_head_detached(commit).map_err(git_message));
    if detached.is_err() {
        let path = worktree.path().to_path_buf();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        }
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).locked(true).working_tree(true);
        let _ = worktree.prune(Some(&mut prune));
        return detached;
    }
    if let Ok(mut temp_branch) = main_repo.find_reference(branch) {
        let _ = temp_branch.delete();
    }
    Ok(())
}

/// Remove a linked worktree: its directory, its registration, its branch,
/// and Holt's merged marker. `force` is the only way past uncommitted
/// changes (the caller confirmed they are disposable).
fn remove_worktree(
    repo: &Repository,
    worktree_name: &str,
    force: bool,
) -> Result<RemovedWorktree, String> {
    let main_repo = Repository::open(repo.commondir()).map_err(git_message)?;
    let worktree = main_repo
        .find_worktree(worktree_name)
        .map_err(|_| format!("worktree {worktree_name} is not registered"))?;
    let path = worktree.path().to_path_buf();
    let present = path.is_dir();
    let branch = if present {
        let worktree_repo = Repository::open(&path).map_err(git_message)?;
        let branch = worktree_repo
            .head()
            .ok()
            .filter(|head| head.is_branch())
            .and_then(|head| head.shorthand().ok().map(str::to_string));
        if !force {
            let mut options = git2::StatusOptions::new();
            options.include_untracked(true);
            let statuses = worktree_repo
                .statuses(Some(&mut options))
                .map_err(git_message)?;
            let dirty: Vec<String> = statuses
                .iter()
                .filter_map(|entry| entry.path().ok().map(str::to_string))
                .collect();
            if !dirty.is_empty() {
                let mut message = format!(
                    "worktree {worktree_name} has uncommitted changes; \
                     pass force to discard them:"
                );
                for path in dirty {
                    message.push('\n');
                    message.push_str(&path);
                }
                return Err(message);
            }
        }
        branch
    } else {
        None
    };
    // A worktree holding the trunk (the repo's default branch) or the main
    // checkout's own branch strands the repository when its branch goes —
    // refuse the whole deletion. (The second half is unreachable through
    // git's own checkout rules — a branch checked out twice is refused —
    // kept as a guard.)
    if let Some(branch) = branch.as_deref() {
        let trunk = local_branch_names(&main_repo).ok().and_then(|names| {
            default_branch(&names, origin_head_target(&main_repo).as_deref()).map(str::to_string)
        });
        let main_branch = current_branch(&main_repo);
        if trunk.as_deref() == Some(branch) || main_branch.as_deref() == Some(branch) {
            return Err(format!(
                "worktree {worktree_name} has {branch} checked out; that branch is \
                 the trunk or the main checkout's branch, so the worktree stays"
            ));
        }
    }

    // Directory first, then the registration: prune refuses a live
    // directory unless told to take it too.
    if present {
        std::fs::remove_dir_all(&path)
            .map_err(|error| format!("could not remove {}: {error}", path.display()))?;
    }
    let mut prune = git2::WorktreePruneOptions::new();
    prune.valid(true).locked(true).working_tree(true);
    worktree
        .prune(Some(&mut prune))
        .map_err(|error| format!("could not prune {worktree_name}: {error}"))?;

    if let Some(branch) = branch.as_deref()
        && let Ok(mut reference) = main_repo.find_reference(&format!("refs/heads/{branch}"))
    {
        reference
            .delete()
            .map_err(|error| format!("could not delete branch {branch}: {error}"))?;
    }
    if let Ok(mut marker) = main_repo.find_reference(&format!("refs/holt/merged/{worktree_name}")) {
        let _ = marker.delete();
    }
    Ok(RemovedWorktree {
        path: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PATCH_BYTES, checkout_identity, classify_status, default_branch, diff_checksum,
        filter_turn_patch, session_branch_name, status_sides, truncate_patch, turn_start_content,
    };
    #[cfg(unix)]
    use super::{normalize_path, resolve_existing};
    #[cfg(unix)]
    use git2::Repository;
    #[cfg(unix)]
    use std::path::Path;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    // ---- session-worktree branch names ----

    #[test]
    fn session_branch_name_is_readable_and_stable() {
        assert_eq!(session_branch_name("chat-1"), "holt/humble-falcon-9111b1c6");
        assert_eq!(session_branch_name("chat-1"), session_branch_name("chat-1"));
        assert_ne!(session_branch_name("chat-1"), session_branch_name("chat-2"));
    }

    // ---- truncate_patch ----

    fn file_section(path: &str, hunks: &[&str]) -> String {
        let mut out = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        for hunk in hunks {
            out.push_str(hunk);
        }
        out
    }

    // Unix-only: creating a symlink on Windows needs a privilege test hosts lack.
    #[cfg(unix)]
    #[test]
    fn repo_relative_resolves_symlinks_and_missing_targets() {
        // A symlinked root (macOS /var/folders → /private/var/folders) with
        // a target that does not exist yet — the write tool's usual state.
        let real = tempfile::TempDir::new().unwrap();
        let link = tempfile::TempDir::new().unwrap();
        let link_path = link.path().join("repo");
        std::os::unix::fs::symlink(real.path(), &link_path).unwrap();

        let repo = Repository::init(real.path()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        drop(repo);

        // Via the symlinked cwd, target absent.
        let candidate = normalize_path(&Path::new(&link_path).join("new-file.txt"));
        let resolved = resolve_existing(&candidate).unwrap();
        let relative = resolved.strip_prefix(&workdir).unwrap();
        assert_eq!(relative, std::path::Path::new("new-file.txt"));

        // And a lexical `..` in the mix still lands inside the work tree.
        let candidate = normalize_path(&Path::new(&link_path).join("sub").join("..").join("b.txt"));
        let resolved = resolve_existing(&candidate).unwrap();
        let relative = resolved.strip_prefix(workdir).unwrap();
        assert_eq!(relative, std::path::Path::new("b.txt"));
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
