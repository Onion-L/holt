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
use holt_proto::{CheckoutDiff, DiffFileSummary, RepoRef};
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
            working_tree_capture(&repo, &device_id)
        })
        .await
    }

    /// Full old/new text for one file in the working-tree capture: old side
    /// from the HEAD blob, new side from the working-tree file. `stale` is
    /// set when the checkout's checksum has moved past the pinned
    /// `diff_checksum` the caller rendered.
    pub(crate) async fn working_tree_file_text(
        &self,
        repo_path: &str,
        device_id: &str,
        request: &holt_proto::GetCheckoutFileDiffTextRequest,
    ) -> Result<holt_proto::CheckoutFileDiffText, String> {
        let device_id = device_id.to_string();
        let request = request.clone();
        self.with_repo(repo_path, move |repo| {
            working_tree_file_text(&repo, &device_id, &request)
        })
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

/// Capture the working tree (HEAD → index → workdir, untracked included,
/// rename-detected) as a wire `CheckoutDiff`.
fn working_tree_capture(repo: &Repository, device_id: &str) -> Result<CheckoutDiff, String> {
    let head = repo.head().ok().and_then(|head| {
        head.peel_to_commit()
            .ok()
            .map(|commit| (commit.id().to_string(), commit.tree().ok()))
    });
    let (head_sha, head_tree) = match head {
        Some((sha, tree)) => (sha, tree),
        None => (EMPTY_HEAD.to_string(), None),
    };

    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        // Untracked deltas carry no content unless asked: the working-tree
        // capture must show new files' additions like `git diff` would.
        .show_untracked_content(true)
        .show_binary(true);
    let mut diff = repo
        .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))
        .map_err(git_message)?;
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).for_untracked(true);
    // Rename detection is best-effort: a failure leaves the raw diff intact.
    let _ = diff.find_similar(Some(&mut find));

    // Per-file summaries: paths/status from the deltas; addition/deletion
    // counts and binary flags from a foreach walk of the diff lines.
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
    // both the binary and line walks annotate the shared summaries.
    let files = std::cell::RefCell::new(files);
    diff.foreach(
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
    )
    .map_err(git_message)?;
    let files = files.into_inner();

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
    let (patch, truncated) = truncate_patch(&patch, MAX_PATCH_BYTES);

    let additions: u32 = files.iter().map(|file| file.additions).sum();
    let deletions: u32 = files.iter().map(|file| file.deletions).sum();
    let workdir = repo
        .workdir()
        .map(|path| path.display().to_string().trim_end_matches('/').to_string())
        .unwrap_or_default();
    Ok(CheckoutDiff {
        checksum: diff_checksum(&head_sha, "workingTree", "", &patch),
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

fn working_tree_file_text(
    repo: &Repository,
    device_id: &str,
    request: &holt_proto::GetCheckoutFileDiffTextRequest,
) -> Result<holt_proto::CheckoutFileDiffText, String> {
    let path = Path::new(&request.path);
    // Old side: the HEAD blob at this path (None for files HEAD doesn't
    // know — adds and renames).
    let old_bytes = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_tree().ok())
        .and_then(|tree| tree.get_path(path).ok())
        .and_then(|entry| entry.to_object(repo).ok())
        .and_then(|object| object.as_blob().map(|blob| blob.content().to_vec()));
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
    let current = working_tree_capture(repo, device_id)?;
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

#[cfg(test)]
mod tests {
    use super::{
        MAX_PATCH_BYTES, checkout_identity, default_branch, diff_checksum, truncate_patch,
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
}
