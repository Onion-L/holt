//! The Git panel's write trio (ADR-0022) over the handle seam: a real
//! engine on a temp data dir, real fixture repositories built with git2,
//! driven through `RpcService` exactly as the UI drives `StagePaths` /
//! `UnstagePaths` / `CommitStaged`. The fixtures carry a repo-local git
//! identity so the commit path never depends on the developer machine's
//! global config — and the missing-identity test blanks it out.

mod common;

use std::path::Path;
use std::time::Duration;

use common::ScriptedProvider;
use futures::StreamExt as _;
use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_proto::{WorkspaceGitStatus, WorkspaceGitStatusKind as Kind};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::json;
use tempfile::TempDir;

/// A body large enough that content similarity behaves predictably.
fn movable_body() -> String {
    let mut body = String::new();
    for line in 0..50 {
        body.push_str(&format!("shared movable content line {line}\n"));
    }
    body
}

/// The repo-local identity every committing fixture gets — the engine
/// reads identity from the effective git config, so the tests supply it
/// the same way a user's `git config` would.
fn set_identity(repo: &Repository) {
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Holt Test").unwrap();
    config
        .set_str("user.email", "holt-test@example.com")
        .unwrap();
}

struct GitFixture {
    /// Where the repository's working tree lives.
    repo_dir: TempDir,
    /// The engine's own data dir.
    data_dir: TempDir,
}

impl GitFixture {
    /// A clean repository on `main` (tracked README + movable body) with a
    /// repo-local git identity configured.
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        set_identity(&repo);
        let movable: &'static str = Box::leak(movable_body().into_boxed_str());
        commit_on(
            &repo,
            "refs/heads/main",
            None,
            &[("README.md", "hello\n"), ("movable.txt", movable)],
            "initial",
        );
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        repo.checkout_head(Some(&mut checkout)).unwrap();
        drop(repo);
        Self {
            repo_dir,
            data_dir: TempDir::new().unwrap(),
        }
    }

    fn repo(&self) -> Repository {
        Repository::open(self.repo_dir.path()).unwrap()
    }

    fn engine(&self) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: Some(ScriptedProvider::new(Vec::new()).stream_fn()),
            search_backend_resolver: None,
        })
        .unwrap()
    }

    fn repo_path(&self) -> String {
        self.repo_dir.path().display().to_string()
    }
}

/// A repository with no commits at all: HEAD points at `main`, unborn.
struct UnbornFixture {
    repo_dir: TempDir,
    data_dir: TempDir,
}

impl UnbornFixture {
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        set_identity(&repo);
        drop(repo);
        Self {
            repo_dir,
            data_dir: TempDir::new().unwrap(),
        }
    }

    fn repo(&self) -> Repository {
        Repository::open(self.repo_dir.path()).unwrap()
    }

    fn engine(&self) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: Some(ScriptedProvider::new(Vec::new()).stream_fn()),
            search_backend_resolver: None,
        })
        .unwrap()
    }

    fn repo_path(&self) -> String {
        self.repo_dir.path().display().to_string()
    }
}

fn signature() -> git2::Signature<'static> {
    git2::Signature::now("Holt Test", "holt-test@example.com").unwrap()
}

/// Commit `entries` (path → content) on top of `parent` onto
/// `update_ref`, without touching the working tree.
fn commit_on(
    repo: &Repository,
    update_ref: &str,
    parent: Option<git2::Oid>,
    entries: &[(&str, &str)],
    message: &str,
) -> git2::Oid {
    let mut builder = match parent {
        Some(parent) => {
            let parent_commit = repo.find_commit(parent).unwrap();
            repo.treebuilder(Some(&parent_commit.tree().unwrap()))
                .unwrap()
        }
        None => repo.treebuilder(None).unwrap(),
    };
    for (path, content) in entries {
        let blob = repo.blob(content.as_bytes()).unwrap();
        builder.insert(*path, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(
        Some(update_ref),
        &signature(),
        &signature(),
        message,
        &tree,
        &parents,
    )
    .unwrap()
}

fn write(repo_dir: &Path, path: &str, contents: &str) {
    let full = repo_dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, contents).unwrap();
}

async fn stage(engine: &LocalEngine, repo_path: &str, paths: &[&str]) -> Result<(), RpcError> {
    match engine
        .handle(
            methods::STAGE_PATHS,
            json!({ "repoPath": repo_path, "paths": paths }),
        )
        .await
    {
        Ok(RpcReply::Value(_)) => Ok(()),
        Ok(_) => panic!("StagePaths did not return a value"),
        Err(error) => Err(error),
    }
}

async fn unstage(engine: &LocalEngine, repo_path: &str, paths: &[&str]) -> Result<(), RpcError> {
    match engine
        .handle(
            methods::UNSTAGE_PATHS,
            json!({ "repoPath": repo_path, "paths": paths }),
        )
        .await
    {
        Ok(RpcReply::Value(_)) => Ok(()),
        Ok(_) => panic!("UnstagePaths did not return a value"),
        Err(error) => Err(error),
    }
}

async fn commit(engine: &LocalEngine, repo_path: &str, message: &str) -> Result<String, RpcError> {
    match engine
        .handle(
            methods::COMMIT_STAGED,
            json!({ "repoPath": repo_path, "message": message }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(value["sha"]
            .as_str()
            .expect("CommitStaged replies with a sha")
            .to_string()),
        Ok(_) => panic!("CommitStaged did not return a value"),
        Err(error) => Err(error),
    }
}

/// Create the Space and chat the status-stream selector expects.
async fn setup_space(engine: &LocalEngine, path: &str) {
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createSpace",
                "spaceId": "space-1",
                "deviceId": engine.engine_info().device_id,
                "path": path,
            }),
        )
        .await
        .unwrap();
}

/// The current status snapshot: a fresh subscription's opening frame. The
/// engine recomputes per subscription, so this needs no timing after a
/// mutation — and dropping the receiver ends the engine-side watch.
async fn snapshot(engine: &LocalEngine) -> WorkspaceGitStatus {
    let RpcReply::Stream(mut items) = engine
        .handle(
            methods::WATCH_WORKSPACE_GIT_STATUS,
            json!({ "spaceId": "space-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchWorkspaceGitStatus did not return a stream");
    };
    let frame = tokio::time::timeout(Duration::from_secs(15), items.next())
        .await
        .expect("snapshot within timeout")
        .expect("snapshot present");
    serde_json::from_value(frame).unwrap()
}

fn entry_of<'a>(
    status: &'a WorkspaceGitStatus,
    path: &str,
) -> Option<&'a holt_proto::WorkspaceGitStatusEntry> {
    status.entries.iter().find(|entry| entry.path == path)
}

// ---- stage / unstage ----

#[tokio::test]
async fn stage_then_unstage_roundtrips_through_the_index() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    write(fixture.repo_dir.path(), "notes.md", "new\n");
    write(fixture.repo_dir.path(), "README.md", "changed\n");

    stage(&engine, &fixture.repo_path(), &["notes.md", "README.md"])
        .await
        .unwrap();
    let repo = fixture.repo();
    let statuses = repo.statuses(None).unwrap();
    let status_of = |path: &str| {
        statuses
            .iter()
            .find(|entry| entry.path().ok() == Some(path))
            .map(|entry| entry.status())
            .unwrap_or(git2::Status::CURRENT)
    };
    assert!(status_of("notes.md").contains(git2::Status::INDEX_NEW));
    assert!(status_of("README.md").contains(git2::Status::INDEX_MODIFIED));

    unstage(&engine, &fixture.repo_path(), &["notes.md"])
        .await
        .unwrap();
    let repo = fixture.repo();
    let statuses = repo.statuses(None).unwrap();
    let status_of = |path: &str| {
        statuses
            .iter()
            .find(|entry| entry.path().ok() == Some(path))
            .map(|entry| entry.status())
            .unwrap_or(git2::Status::CURRENT)
    };
    assert_eq!(status_of("notes.md"), git2::Status::WT_NEW);
    assert!(
        status_of("README.md").contains(git2::Status::INDEX_MODIFIED),
        "the other path stays staged"
    );
}

#[tokio::test]
async fn stage_and_unstage_reject_bad_paths() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();

    for paths in [
        vec!["/etc/passwd"],
        vec!["../outside.txt"],
        vec!["ok.txt", "../sneaky.txt"],
        vec![""],
        vec!["   "],
    ] {
        let error = stage(&engine, &fixture.repo_path(), &paths)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RpcError::BadParams(_)),
            "stage {paths:?}: {error}"
        );
        let error = unstage(&engine, &fixture.repo_path(), &paths)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RpcError::BadParams(_)),
            "unstage {paths:?}: {error}"
        );
    }

    // An empty list is bad params for both calls.
    for call in [methods::STAGE_PATHS, methods::UNSTAGE_PATHS] {
        let error = match engine
            .handle(
                call,
                json!({ "repoPath": fixture.repo_path(), "paths": [] }),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("{call} with an empty path list must fail"),
        };
        assert!(matches!(error, RpcError::BadParams(_)), "{call}: {error}");
    }
}

#[tokio::test]
async fn unstaging_a_path_absent_from_the_index_is_a_noop() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();

    // Clean tracked file (nothing staged) and a path the repo never heard
    // of: both succeed and change nothing.
    unstage(&engine, &fixture.repo_path(), &["README.md"])
        .await
        .unwrap();
    unstage(&engine, &fixture.repo_path(), &["never-existed.txt"])
        .await
        .unwrap();
    let repo = fixture.repo();
    assert!(repo.statuses(None).unwrap().is_empty(), "still clean");
}

// ---- commit ----

#[tokio::test]
async fn commit_staged_commits_the_index_and_returns_the_sha() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    write(fixture.repo_dir.path(), "README.md", "panel work\n");
    stage(&engine, &fixture.repo_path(), &["README.md"])
        .await
        .unwrap();

    let sha = commit(&engine, &fixture.repo_path(), "panel commit\n")
        .await
        .unwrap();

    let repo = fixture.repo();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(
        head.id().to_string(),
        sha,
        "HEAD advanced to the new commit"
    );
    assert_eq!(head.message().ok(), Some("panel commit\n"));
    // Identity came from the repo config; author equals committer.
    assert_eq!(head.author().name().ok(), Some("Holt Test"));
    assert_eq!(head.author().email().ok(), Some("holt-test@example.com"));
    assert_eq!(
        head.author().when().seconds(),
        head.committer().when().seconds()
    );
    assert_eq!(head.committer().email(), head.author().email());
    let parent = head.parent(0).unwrap();
    assert_eq!(parent.message().ok(), Some("initial"));
    // The staged change is in the tree; the index is clean again.
    let head_tree = head.tree().unwrap();
    let blob = head_tree.get_name("README.md").unwrap();
    let object = blob.to_object(&repo).unwrap();
    assert_eq!(
        std::str::from_utf8(object.as_blob().unwrap().content()).unwrap(),
        "panel work\n"
    );
    assert!(repo.statuses(None).unwrap().is_empty());
}

#[tokio::test]
async fn commit_rejects_a_blank_message_and_an_empty_index() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();

    for message in ["", "   ", "\n"] {
        let error = commit(&engine, &fixture.repo_path(), message)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RpcError::BadParams(_)),
            "blank message {message:?}: {error}"
        );
    }

    let error = commit(&engine, &fixture.repo_path(), "nothing to say")
        .await
        .unwrap_err();
    assert!(
        matches!(error, RpcError::Failed(ref message) if message.contains("nothing staged")),
        "nothing staged fails like git: {error}"
    );
}

#[tokio::test]
async fn commit_without_a_git_identity_fails_actionably() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    // Blank local values shadow any global config the machine may have:
    // the effective identity is missing regardless of the environment.
    let repo = fixture.repo();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "").unwrap();
    config.set_str("user.email", "").unwrap();
    drop(config);
    drop(repo);

    write(fixture.repo_dir.path(), "README.md", "no identity\n");
    stage(&engine, &fixture.repo_path(), &["README.md"])
        .await
        .unwrap();
    let error = commit(&engine, &fixture.repo_path(), "who am i")
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, RpcError::Failed(_)));
    assert!(
        message.contains("user.name") && message.contains("user.email"),
        "the message tells the user what to configure: {message}"
    );
    // The refusal created no commit.
    let repo = fixture.repo();
    assert_eq!(
        repo.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .ok(),
        Some("initial")
    );
}

#[tokio::test]
async fn staging_a_deleted_tracked_file_records_a_removal() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    std::fs::remove_file(fixture.repo_dir.path().join("movable.txt")).unwrap();

    stage(&engine, &fixture.repo_path(), &["movable.txt"])
        .await
        .unwrap();
    let repo = fixture.repo();
    assert!(
        repo.index()
            .unwrap()
            .get_path(Path::new("movable.txt"), 0)
            .is_none(),
        "the deletion is staged as a remove-from-index, not an add"
    );

    let sha = commit(&engine, &fixture.repo_path(), "remove movable")
        .await
        .unwrap();
    let repo = fixture.repo();
    let commit = repo
        .find_commit(git2::Oid::from_str(&sha).unwrap())
        .unwrap();
    assert!(
        commit.tree().unwrap().get_name("movable.txt").is_none(),
        "the commit removes the file from the tree"
    );
}

#[tokio::test]
async fn detached_head_commit_advances_head_and_moves_no_branch() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    let tip = {
        let repo = fixture.repo();
        let tip = repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.set_head_detached(tip).unwrap();
        tip
    };

    write(fixture.repo_dir.path(), "detached.txt", "detached work\n");
    stage(&engine, &fixture.repo_path(), &["detached.txt"])
        .await
        .unwrap();
    let sha = commit(&engine, &fixture.repo_path(), "detached commit")
        .await
        .unwrap();

    let repo = fixture.repo();
    assert!(repo.head_detached().unwrap(), "HEAD stays detached");
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.id().to_string(), sha);
    assert_eq!(head.parent_id(0).unwrap(), tip, "built on the detached tip");
    let main = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(main, tip, "no branch moved");
}

// ---- unborn HEAD ----

#[tokio::test]
async fn unborn_head_stage_and_commit_create_the_root_commit() {
    let fixture = UnbornFixture::new();
    let engine = fixture.engine();
    write(fixture.repo_dir.path(), "seed.txt", "first\n");

    stage(&engine, &fixture.repo_path(), &["seed.txt"])
        .await
        .unwrap();
    let sha = commit(&engine, &fixture.repo_path(), "root commit")
        .await
        .unwrap();

    let repo = fixture.repo();
    let commit = repo
        .find_commit(git2::Oid::from_str(&sha).unwrap())
        .unwrap();
    assert_eq!(commit.parent_count(), 0, "a parentless root commit");
    assert_eq!(
        repo.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string(),
        sha,
        "main was born at the root commit"
    );
    assert!(commit.tree().unwrap().get_name("seed.txt").is_some());
}

#[tokio::test]
async fn unborn_head_unstage_drops_paths_from_the_index() {
    let fixture = UnbornFixture::new();
    let engine = fixture.engine();
    write(fixture.repo_dir.path(), "a.txt", "a\n");
    write(fixture.repo_dir.path(), "dir/b.txt", "b\n");

    stage(&engine, &fixture.repo_path(), &["a.txt", "dir"])
        .await
        .unwrap();
    unstage(&engine, &fixture.repo_path(), &["dir"])
        .await
        .unwrap();

    let repo = fixture.repo();
    let index = repo.index().unwrap();
    assert!(
        index.get_path(Path::new("a.txt"), 0).is_some(),
        "a.txt stays staged"
    );
    assert!(
        index.get_path(Path::new("dir/b.txt"), 0).is_none(),
        "the directory's entries drop out of the index"
    );
    // An absent path on an unborn HEAD is still a no-op success.
    unstage(&engine, &fixture.repo_path(), &["never-existed.txt"])
        .await
        .unwrap();
}

// ---- merge-state and conflict gates ----

/// Put the fixture mid-merge with a conflicted README: `side` and `main`
/// both rewrote README over the shared base, and the merge stops on the
/// conflict with MERGE_HEAD written.
fn start_conflicting_merge(fixture: &GitFixture) {
    let repo = fixture.repo();
    let movable = movable_body();
    let base = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    commit_on(
        &repo,
        "refs/heads/side",
        Some(base),
        &[("README.md", "side\n"), ("movable.txt", &movable)],
        "side work",
    );
    commit_on(
        &repo,
        "refs/heads/main",
        Some(base),
        &[("README.md", "main\n"), ("movable.txt", &movable)],
        "main work",
    );
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout)).unwrap();
    let side_oid = repo
        .find_reference("refs/heads/side")
        .unwrap()
        .target()
        .unwrap();
    let side = repo.find_annotated_commit(side_oid).unwrap();
    repo.merge(&[&side], None, None).unwrap();
    assert_eq!(repo.state(), git2::RepositoryState::Merge);
    assert!(repo.index().unwrap().has_conflicts());
}

#[tokio::test]
async fn commit_is_refused_mid_merge_even_with_conflicts_resolved() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    start_conflicting_merge(&fixture);

    let error = commit(&engine, &fixture.repo_path(), "conclude the merge")
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, RpcError::Failed(_)));
    assert!(message.contains("merge"), "names the operation: {message}");

    // The disguise case: resolve the conflict (the index is clean of
    // conflicts) but never conclude — MERGE_HEAD persists, so committing
    // must still refuse or the merge parentage would be lost.
    write(fixture.repo_dir.path(), "README.md", "resolved\n");
    let repo = fixture.repo();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    assert!(!repo.index().unwrap().has_conflicts());
    assert_eq!(repo.state(), git2::RepositoryState::Merge);
    drop(index);
    drop(repo);

    let error = commit(&engine, &fixture.repo_path(), "conclude the merge")
        .await
        .unwrap_err();
    assert!(
        matches!(error, RpcError::Failed(ref message) if message.contains("merge")),
        "resolved-but-unconcluded merge still refuses: {error}"
    );
    // Nothing was committed: HEAD is still the pre-merge main tip.
    let repo = fixture.repo();
    assert_eq!(
        repo.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .ok(),
        Some("main work")
    );
}

#[tokio::test]
async fn staging_or_unstaging_a_conflicted_path_is_refused() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    start_conflicting_merge(&fixture);

    let error = stage(&engine, &fixture.repo_path(), &["README.md"])
        .await
        .unwrap_err();
    assert!(
        matches!(error, RpcError::Failed(ref message) if message.contains("conflict")),
        "staging a conflicted path refuses: {error}"
    );
    let error = unstage(&engine, &fixture.repo_path(), &["README.md"])
        .await
        .unwrap_err();
    assert!(
        matches!(error, RpcError::Failed(ref message) if message.contains("conflict")),
        "unstaging a conflicted path refuses: {error}"
    );
    // The conflict is untouched: still conflicted, still mid-merge.
    let repo = fixture.repo();
    assert!(repo.index().unwrap().has_conflicts());
    assert_eq!(repo.state(), git2::RepositoryState::Merge);
}

// ---- status stream integration ----

#[tokio::test]
async fn staging_an_untracked_directory_adds_it_recursively() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;
    write(fixture.repo_dir.path(), "bundle/a.txt", "a\n");
    write(fixture.repo_dir.path(), "bundle/nested/b.txt", "b\n");

    stage(&engine, &fixture.repo_path(), &["bundle"])
        .await
        .unwrap();

    // The status stream reports the staged files individually — the
    // collapsed whole-directory entry is gone.
    let status = snapshot(&engine).await;
    for path in ["bundle/a.txt", "bundle/nested/b.txt"] {
        let entry = entry_of(&status, path)
            .unwrap_or_else(|| panic!("{path} missing from {:?}", status.entries));
        assert_eq!(entry.kind, Kind::Added, "{path}");
        assert_eq!(entry.index, Some(Kind::Added), "{path}");
        assert_eq!(entry.worktree, None, "{path}");
    }
    assert!(
        entry_of(&status, "bundle").is_none(),
        "no collapsed directory entry once its files are tracked"
    );
}
