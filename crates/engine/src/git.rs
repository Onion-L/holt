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
use holt_proto::RepoRef;

/// Serves the git surface for space folders. One per engine.
#[derive(Default)]
pub(crate) struct Git {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
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

    /// Run `op` against the repository resolved from `repo_path`: resolve
    /// its common git dir, take the per-checkout lock, then execute on the
    /// blocking pool.
    async fn with_repo<T, F>(&self, repo_path: &str, op: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(Repository) -> Result<T, String> + Send + 'static,
    {
        let repo_path = repo_path.to_string();
        let discover_path = repo_path.clone();
        let lock_key = tokio::task::spawn_blocking(move || {
            let repo = Repository::discover(&discover_path).map_err(git_message)?;
            Ok::<_, String>(normalize(repo.commondir()))
        })
        .await
        .map_err(|error| error.to_string())??;
        let guard = self.lock_for(lock_key).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let repo = Repository::discover(&repo_path).map_err(git_message)?;
            op(repo)
        })
        .await
        .map_err(|error| error.to_string())?
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

/// Create nothing, merge nothing, stash nothing: resolve the branch, check
/// its tree out with git's SAFE strategy, then move HEAD. A checkout that
/// would clobber uncommitted data fails before HEAD moves, with git's
/// message verbatim.
fn switch_branch(repo: &Repository, branch_name: &str) -> Result<(), String> {
    let branch = repo
        .find_branch(branch_name, BranchType::Local)
        .map_err(git_message)?;
    let reference = branch.into_reference();
    let ref_name = reference.name().map_err(git_message)?.to_string();
    let tree = reference
        .peel(git2::ObjectType::Tree)
        .map_err(git_message)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(&tree, Some(&mut checkout))
        .map_err(git_message)?;
    repo.set_head(&ref_name).map_err(git_message)?;
    Ok(())
}

fn git_message(error: git2::Error) -> String {
    error.message().to_string()
}

/// Collapse a git dir to its component form so `.git/` and `.git` hash as
/// the same lock key.
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use super::default_branch;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
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
}
