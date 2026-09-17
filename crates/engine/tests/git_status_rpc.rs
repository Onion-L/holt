//! Working-tree Git status for the File sidebar (ticket 10) over the
//! handle seam: a real engine on a temp data dir, a real fixture repository
//! built with git2, driven through `RpcService` exactly as the UI drives
//! `WatchWorkspaceGitStatus`. Every fresh subscription opens with the
//! current snapshot, so most tests re-subscribe after each mutation — no
//! timing — and one test holds a stream open to exercise the live frames.

mod common;

use std::time::Duration;

use common::{Fixture, ScriptedProvider};
use futures::StreamExt as _;
use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_proto::{WorkspaceGitStatus, WorkspaceGitStatusKind as Kind};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::json;
use tempfile::TempDir;

/// Bodies large enough for git's rename similarity to pair moves reliably.
fn movable_body(prefix: &str) -> String {
    let mut body = String::new();
    for line in 0..50 {
        body.push_str(&format!("{prefix} movable content line {line}\n"));
    }
    body
}

struct GitFixture {
    /// Where the repository's working tree lives.
    repo_dir: TempDir,
    /// The engine's own data dir.
    data_dir: TempDir,
}

impl GitFixture {
    /// A clean repository on `main`: a tracked README and a
    /// rename-detectable movable body (tree entries stay flat — git
    /// treebuilders do not take nested paths).
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let movable: &'static str = Box::leak(movable_body("shared").into_boxed_str());
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

    /// A real engine (no provider traffic needed — the status surface
    /// never runs a Turn).
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

/// Commit `entries` (path → content) on top of `parent` onto `update_ref`,
/// without touching the working tree.
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

/// The current status snapshot: a fresh subscription's opening frame. The
/// engine recomputes per subscription, so this needs no timing after a
/// mutation — and dropping the receiver ends the engine-side watch.
async fn snapshot(engine: &LocalEngine, selector: serde_json::Value) -> WorkspaceGitStatus {
    let RpcReply::Stream(mut items) = engine
        .handle(methods::WATCH_WORKSPACE_GIT_STATUS, selector)
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

async fn next_frame(
    items: &mut (impl futures::Stream<Item = serde_json::Value> + Unpin),
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(15), items.next())
        .await
        .expect("frame within timeout")
        .expect("frame present")
}

fn kind_of(status: &WorkspaceGitStatus, path: &str) -> Option<Kind> {
    entry_of(status, path).map(|entry| entry.kind)
}

fn entry_of<'a>(
    status: &'a WorkspaceGitStatus,
    path: &str,
) -> Option<&'a holt_proto::WorkspaceGitStatusEntry> {
    status.entries.iter().find(|entry| entry.path == path)
}

/// Create the Space and chat the selector family expects, rooted at `path`.
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
    engine
        .handle(
            methods::MUTATE,
            json!({ "op": "createChat", "chatId": "chat-1", "spaceId": "space-1" }),
        )
        .await
        .unwrap();
}

fn write(repo_dir: &std::path::Path, path: &str, contents: &str) {
    let full = repo_dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, contents).unwrap();
}

fn stage(repo: &Repository, paths: &[&str]) {
    let mut index = repo.index().unwrap();
    for path in paths {
        index.add_path(std::path::Path::new(path)).unwrap();
    }
    index.write().unwrap();
}

fn canonical(path: &std::path::Path) -> String {
    std::fs::canonicalize(path).unwrap().display().to_string()
}

/// The plain-fixture engine + space (the common::Fixture writes its own
/// directories but initializes no repository).
async fn non_git_engine() -> (Fixture, LocalEngine) {
    let fixture = Fixture::new();
    let engine = fixture.engine(&ScriptedProvider::new(Vec::new()));
    setup_space(&engine, &fixture.cwd()).await;
    (fixture, engine)
}

#[tokio::test]
async fn clean_repository_reports_an_empty_clean_snapshot() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    for selector in [
        json!({ "chatId": "chat-1" }),
        json!({ "spaceId": "space-1" }),
    ] {
        let status = snapshot(&engine, selector.clone()).await;
        assert!(status.entries.is_empty(), "clean tree, {:?}", selector);
        assert_eq!(status.error, None);
        // The workdir is canonical so the tree's canonical row paths strip
        // against it cleanly.
        let expected = canonical(fixture.repo_dir.path());
        assert_eq!(status.workdir.as_deref(), Some(expected.as_str()));
    }
}

#[tokio::test]
async fn untracked_modified_and_hidden_entries_classify() {
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, "notes.md", "new\n");
    write(&repo_dir, ".hidden/note.txt", "hidden but not ignored\n");
    std::fs::write(repo_dir.join("README.md"), "changed\n").unwrap();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(kind_of(&status, "notes.md"), Some(Kind::Untracked));
    // A hidden directory without an ignore rule is untracked, not ignored —
    // reported whole (git does not recurse into untracked directories).
    assert_eq!(kind_of(&status, ".hidden"), Some(Kind::Untracked));
    assert_eq!(kind_of(&status, "README.md"), Some(Kind::Modified));
    // Paths normalize: no libgit2 trailing `/` survives the wire; the
    // whole-directory fact survives as the is-dir flag instead.
    assert!(
        status
            .entries
            .iter()
            .all(|entry| !entry.path.ends_with('/'))
    );
    assert!(entry_of(&status, ".hidden").unwrap().is_dir);
    assert!(!entry_of(&status, "notes.md").unwrap().is_dir);
    assert!(!entry_of(&status, "README.md").unwrap().is_dir);
}

#[tokio::test]
async fn ignored_directories_and_files_report_whole() {
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, ".gitignore", "node_modules/\n*.log\n");
    stage(&fixture.repo(), &[".gitignore"]);
    write(&repo_dir, "node_modules/react/index.js", "deps\n");
    write(&repo_dir, "debug.log", "noise\n");
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    // One entry for the whole ignored directory — no recursion inside it,
    // so expanding a large ignored directory never grows this list.
    let node_modules = entry_of(&status, "node_modules").unwrap();
    assert_eq!(node_modules.kind, Kind::Ignored);
    assert!(node_modules.is_dir);
    assert!(
        status
            .entries
            .iter()
            .all(|entry| !entry.path.starts_with("node_modules/"))
    );
    assert_eq!(entry_of(&status, "debug.log").unwrap().kind, Kind::Ignored);
    assert!(!entry_of(&status, "debug.log").unwrap().is_dir);
    // The staged .gitignore is itself a change the tree sees.
    assert_eq!(kind_of(&status, ".gitignore"), Some(Kind::Added));
}

#[tokio::test]
async fn moves_decorate_under_the_new_name() {
    // Rename detection is off by design: libgit2 keys detected renames at
    // the OLD path, which a live tree cannot show. A moved file carries
    // its marker under the new name — untracked when the move is unstaged,
    // added once staged.
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, "new.rs", "fresh\n");
    std::fs::rename(repo_dir.join("movable.txt"), repo_dir.join("moved.txt")).unwrap();
    stage(&fixture.repo(), &["new.rs"]);

    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;
    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    // Staged new file vs plain untracked.
    assert_eq!(kind_of(&status, "new.rs"), Some(Kind::Added));
    // The unstaged move: gone from the old name (reported Deleted there,
    // invisible to the tree) and untracked under the new one.
    assert_eq!(kind_of(&status, "moved.txt"), Some(Kind::Untracked));
    assert_eq!(kind_of(&status, "movable.txt"), Some(Kind::Deleted));
}

#[tokio::test]
async fn staged_then_modified_reports_both_porcelain_sides() {
    // MM: modify a tracked file, stage it, modify again — the entry carries
    // Modified on BOTH sides while the collapsed kind stays Modified.
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    std::fs::write(repo_dir.join("README.md"), "edit one\n").unwrap();
    stage(&fixture.repo(), &["README.md"]);
    std::fs::write(repo_dir.join("README.md"), "edit two\n").unwrap();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    let entry = entry_of(&status, "README.md").expect("README.md entry");
    assert_eq!(entry.kind, Kind::Modified);
    assert_eq!(entry.index, Some(Kind::Modified));
    assert_eq!(entry.worktree, Some(Kind::Modified));
    assert!(!entry.is_dir);
}

#[tokio::test]
async fn staged_new_file_then_modified_reports_added_and_modified() {
    // AM: a new file staged then modified — index side Added, worktree
    // side Modified; the collapsed kind keeps its Added precedence.
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, "fresh.rs", "v1\n");
    stage(&fixture.repo(), &["fresh.rs"]);
    std::fs::write(repo_dir.join("fresh.rs"), "v2\n").unwrap();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    let entry = entry_of(&status, "fresh.rs").expect("fresh.rs entry");
    assert_eq!(entry.kind, Kind::Added);
    assert_eq!(entry.index, Some(Kind::Added));
    assert_eq!(entry.worktree, Some(Kind::Modified));
}

#[tokio::test]
async fn untracked_entries_report_the_worktree_side_only() {
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, "notes.md", "new\n");
    write(&repo_dir, "drafts/a.md", "whole dir\n");
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    let file = entry_of(&status, "notes.md").expect("notes.md entry");
    assert_eq!(file.index, None);
    assert_eq!(file.worktree, Some(Kind::Untracked));
    let dir = entry_of(&status, "drafts").expect("drafts entry");
    assert_eq!(dir.index, None);
    assert_eq!(dir.worktree, Some(Kind::Untracked));
    assert!(dir.is_dir);
}

#[tokio::test]
async fn a_merge_conflict_reports_conflicted_not_modified() {
    // main and side both rewrite movable.txt; merging side into main
    // leaves an unmerged path the snapshot must report honestly.
    let fixture = GitFixture::new();
    let repo = fixture.repo();
    let base = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let side = commit_on(
        &repo,
        "refs/heads/side",
        Some(base),
        &[
            ("README.md", "hello\n"),
            ("movable.txt", movable_body("side").as_str()),
        ],
        "side change",
    );
    let main = commit_on(
        &repo,
        "refs/heads/main",
        Some(base),
        &[
            ("README.md", "hello\n"),
            ("movable.txt", movable_body("main").as_str()),
        ],
        "main change",
    );
    // commit_on moves the ref only; sync index and worktree to the new
    // HEAD before merging so the merge's checkout sees a clean tree.
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.reset(
        repo.find_commit(main).unwrap().as_object(),
        git2::ResetType::Hard,
        Some(&mut checkout),
    )
    .unwrap();
    let annotated = repo.find_annotated_commit(side).unwrap();
    repo.merge(&[&annotated], None, None).unwrap();
    drop(annotated);
    drop(repo);

    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;
    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(status.error, None);
    let entry = entry_of(&status, "movable.txt").expect("movable.txt entry");
    assert_eq!(entry.kind, Kind::Conflicted);
    // The sides stay honest with the collapsed kind: porcelain fills both
    // columns for an unmerged path.
    assert_eq!(entry.index, Some(Kind::Conflicted));
    assert_eq!(entry.worktree, Some(Kind::Conflicted));
}

#[tokio::test]
async fn non_git_space_streams_a_null_workdir_without_error() {
    let (_fixture, engine) = non_git_engine().await;
    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(status.workdir, None);
    assert!(status.entries.is_empty());
    assert_eq!(status.error, None, "not a repo is a state, not a failure");
}

#[tokio::test]
async fn a_space_inside_the_repo_sees_repo_relative_status() {
    // The Space's folder is a subdirectory of the worktree: statuses still
    // key repo-relative against the WORKTREE's workdir, so the UI's strip
    // keeps working. `src` itself is untracked on disk — enough: the point
    // is the workdir and the key spelling, not tracked-ness.
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    write(&repo_dir, "src/main.rs", "fn main() {}\n");
    let engine = fixture.engine();
    setup_space(&engine, &repo_dir.join("src").display().to_string()).await;

    let status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    let expected = canonical(&repo_dir);
    assert_eq!(status.workdir.as_deref(), Some(expected.as_str()));
    assert_eq!(kind_of(&status, "src"), Some(Kind::Untracked));
}

#[tokio::test]
async fn branch_switches_move_decorations() {
    // feature commits a .gitignore that ignores debug.log; main does not.
    // The untracked file becomes ignored after the switch — a status change
    // the tree must see without inheriting any diff scope.
    let fixture = GitFixture::new();
    let repo = fixture.repo();
    let base = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let movable = movable_body("shared");
    commit_on(
        &repo,
        "refs/heads/feature",
        Some(base),
        &[
            ("README.md", "hello\n"),
            ("movable.txt", movable.as_str()),
            (".gitignore", "debug.log\n"),
        ],
        "ignore logs on feature",
    );
    drop(repo);
    write(fixture.repo_dir.path(), "debug.log", "noise\n");

    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;
    let before = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(kind_of(&before, "debug.log"), Some(Kind::Untracked));

    engine
        .handle(
            methods::SWITCH_REF,
            json!({ "repoPath": fixture.repo_path(), "refName": "feature" }),
        )
        .await
        .unwrap();
    let after = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(kind_of(&after, "debug.log"), Some(Kind::Ignored));
}

#[tokio::test]
async fn linked_worktrees_carry_their_own_status() {
    // ADR-0002: no parallel registry — a worktree Space discovers its own
    // checkout, and its status sees only its own folder.
    let fixture = GitFixture::new();
    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path().join("wt");
    let repo = fixture.repo();
    let _worktree_repo = repo.worktree("wt", &worktree_root, None).unwrap();
    drop(repo);

    // Dirty the MAIN worktree only.
    write(fixture.repo_dir.path(), "main-only.txt", "dirty\n");
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createSpace",
                "spaceId": "space-2",
                "deviceId": engine.engine_info().device_id,
                "path": worktree_root.display().to_string(),
            }),
        )
        .await
        .unwrap();

    let main_status = snapshot(&engine, json!({ "spaceId": "space-1" })).await;
    assert_eq!(
        kind_of(&main_status, "main-only.txt"),
        Some(Kind::Untracked)
    );

    let worktree_status = snapshot(&engine, json!({ "spaceId": "space-2" })).await;
    let expected = canonical(&worktree_root);
    assert_eq!(
        worktree_status.workdir.as_deref(),
        Some(expected.as_str()),
        "the worktree's own workdir"
    );
    assert!(
        worktree_status.entries.is_empty(),
        "the main worktree's dirt stays out: {:?}",
        worktree_status.entries
    );
}

#[tokio::test]
async fn selector_must_be_exactly_one_of_chat_or_space() {
    let fixture = GitFixture::new();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

    for params in [
        json!({}),
        json!({ "chatId": "chat-1", "spaceId": "space-1" }),
    ] {
        let error = match engine
            .handle(methods::WATCH_WORKSPACE_GIT_STATUS, params)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("selector validation must reject this shape"),
        };
        assert!(matches!(error, RpcError::BadParams(_)), "{error}");
    }
    let error = match engine
        .handle(
            methods::WATCH_WORKSPACE_GIT_STATUS,
            json!({ "spaceId": "nope" }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an unknown selector must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)), "{error}");
}

#[tokio::test]
async fn live_frames_follow_external_writes_and_staging() {
    // The timing test: hold one stream open, mutate on disk (an external
    // editor), then stage (a `.git` change the checkout-diff watch's
    // checksum gate would not see), and expect a fresh frame after each.
    let fixture = GitFixture::new();
    let repo_dir = fixture.repo_dir.path().to_path_buf();
    let engine = fixture.engine();
    setup_space(&engine, &fixture.repo_path()).await;

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

    let opening: WorkspaceGitStatus = serde_json::from_value(next_frame(&mut items).await).unwrap();
    assert!(opening.entries.is_empty());

    // External write: untracked appears live.
    write(&repo_dir, "live.md", "fresh\n");
    let after_write: WorkspaceGitStatus =
        serde_json::from_value(next_frame(&mut items).await).unwrap();
    assert_eq!(kind_of(&after_write, "live.md"), Some(Kind::Untracked));

    // Staging: a `.git/index` change — the marker moves U → A.
    stage(&fixture.repo(), &["live.md"]);
    let after_stage: WorkspaceGitStatus =
        serde_json::from_value(next_frame(&mut items).await).unwrap();
    assert_eq!(kind_of(&after_stage, "live.md"), Some(Kind::Added));
}
