//! The integration view's read surface (`ListIntegration`): one repo, its
//! trunk, and every linked worktree — driven through the `RpcService` trait
//! exactly as the UI drives it, against a real fixture repository built with
//! git2.

use crate::common;

use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_proto::IntegrationSnapshot;
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

struct Fixture {
    /// Where the repository's main working tree lives.
    repo_dir: TempDir,
    /// The engine's own data dir.
    data_dir: TempDir,
}

impl Fixture {
    /// main: `root` ← `base`; the main checkout sits on `main`, clean.
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = common::init_repo(repo_dir.path());
        repo.set_head("refs/heads/main").unwrap();
        common::commit_on(
            &repo,
            "refs/heads/main",
            None,
            &[("README.md", "hello\n")],
            "root",
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
            stream_fn: Some(
                common::ScriptedProvider::new(
                    (0..4)
                        .map(|_| common::ScriptedReply::text("done"))
                        .collect(),
                )
                .stream_fn(),
            ),
            search_backend_resolver: None,
            clock: None,
            exec_shell: None,
        })
        .unwrap()
    }

    fn repo_path(&self) -> String {
        self.repo_dir.path().display().to_string()
    }

    /// An engine whose model never answers — the Turn hangs open, for the
    /// delete-while-running refusal.
    fn silent_engine(&self) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: Some(
                common::ScriptedProvider::new(vec![common::ScriptedReply::Silent]).stream_fn(),
            ),
            search_backend_resolver: None,
            clock: None,
            exec_shell: None,
        })
        .unwrap()
    }
}

/// Add a linked worktree at a fresh child of `parent_dir`, on branch
/// `branch` (created at `base` when missing), and return its path.
fn add_worktree(
    repo: &Repository,
    parent_dir: &TempDir,
    name: &str,
    branch: &str,
    base: &str,
) -> String {
    let path = parent_dir.path().join(name);
    let reference = match repo.find_reference(&format!("refs/heads/{branch}")) {
        Ok(reference) => reference,
        Err(_) => {
            let base_commit = repo
                .find_reference(&format!("refs/heads/{base}"))
                .unwrap()
                .peel_to_commit()
                .unwrap();
            repo.branch(branch, &base_commit, false)
                .unwrap()
                .into_reference()
        }
    };
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repo.worktree(name, &path, Some(&options)).unwrap();
    path.display().to_string()
}

async fn list_integration(
    engine: &LocalEngine,
    repo_path: &str,
    trunk: Option<&str>,
) -> Result<IntegrationSnapshot, RpcError> {
    let mut params = serde_json::json!({ "repoPath": repo_path });
    if let Some(trunk) = trunk {
        params["trunk"] = serde_json::Value::String(trunk.to_string());
    }
    match engine.handle(methods::LIST_INTEGRATION, params).await {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("ListIntegration did not return a value"),
        Err(error) => Err(error),
    }
}

fn worktree<'a>(
    snapshot: &'a IntegrationSnapshot,
    name: &str,
) -> &'a holt_proto::IntegrationWorktree {
    snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.name == name)
        .unwrap_or_else(|| panic!("{name} missing from the snapshot"))
}

/// `git rev-list --left-right --count trunk...branch` via git2's own graph
/// walk — the cross-check the acceptance asks for, computed independently of
/// the implementation's `graph_ahead_behind` call.
fn ahead_behind_by_walk(repo: &Repository, trunk: &str, branch: &str) -> (usize, usize) {
    let trunk_oid = repo
        .find_reference(&format!("refs/heads/{trunk}"))
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let branch_oid = repo
        .find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let count_reachable = |exclude: git2::Oid, from: git2::Oid| {
        let mut walker = repo.revwalk().unwrap();
        walker.push(from).unwrap();
        walker.hide(exclude).unwrap();
        walker.count()
    };
    (
        count_reachable(trunk_oid, branch_oid),
        count_reachable(branch_oid, trunk_oid),
    )
}

#[tokio::test]
async fn ahead_behind_and_fork_match_the_graph() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // ahead-only: branches straight off main's tip.
    let ahead_path = add_worktree(&repo, &wt_dir, "wt-ahead", "wt-ahead", "main");
    {
        let ahead_repo = Repository::open(&ahead_path).unwrap();
        common::commit_on(
            &ahead_repo,
            "refs/heads/wt-ahead",
            Some(main_tip),
            &[("README.md", "hello\n"), ("ahead.txt", "ahead\n")],
            "ahead work",
        );
    }
    // ahead-and-behind: branches off the root, main moves on after.
    let mixed_path = add_worktree(&repo, &wt_dir, "wt-mixed", "wt-mixed", "main");
    {
        let mixed_repo = Repository::open(&mixed_path).unwrap();
        common::commit_on(
            &mixed_repo,
            "refs/heads/wt-mixed",
            Some(main_tip),
            &[("README.md", "hello\n"), ("mixed.txt", "mixed\n")],
            "mixed work",
        );
    }
    let moved_tip = common::commit_on(
        &repo,
        "refs/heads/main",
        Some(main_tip),
        &[("README.md", "hello\n"), ("trunk.txt", "trunk work\n")],
        "trunk moves",
    );
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();

    let repo = fixture.repo();
    let (want_ahead, want_behind) = ahead_behind_by_walk(&repo, "main", "wt-ahead");
    let ahead = worktree(&snapshot, "wt-ahead");
    assert_eq!((ahead.ahead, ahead.behind), (want_ahead, want_behind));
    assert_eq!((ahead.ahead, ahead.behind), (1, 1), "main moved once");
    assert_eq!(
        ahead.fork_sha.as_deref(),
        Some(main_tip.to_string().as_str()),
        "fork is the merge-base with main's original tip"
    );

    let (want_ahead, want_behind) = ahead_behind_by_walk(&repo, "main", "wt-mixed");
    let mixed = worktree(&snapshot, "wt-mixed");
    assert_eq!((mixed.ahead, mixed.behind), (want_ahead, want_behind));
    assert_eq!(
        mixed.fork_sha.as_deref(),
        Some(main_tip.to_string().as_str())
    );
    // The per-worktree range is exactly trunk..head: the branch's own
    // commit, not the trunk's.
    let shas: Vec<&str> = mixed.commits.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas.len(), 1);
    assert_eq!(shas[0], mixed.head_sha.as_deref().unwrap());
    // Trunk rows run newest first from the trunk head.
    assert_eq!(snapshot.trunk.head_sha, moved_tip.to_string());
    assert_eq!(snapshot.trunk.name, "main");
    assert_eq!(snapshot.trunk_commits[0].sha, moved_tip.to_string());
    assert!(
        snapshot
            .trunk_commits
            .iter()
            .any(|c| c.sha == main_tip.to_string())
    );
}

#[tokio::test]
async fn dirty_tracks_untracked_but_not_ignored_files() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let path = add_worktree(&repo, &wt_dir, "wt-dirty", "wt-dirty", "main");
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert!(!worktree(&snapshot, "wt-dirty").dirty, "clean worktree");

    // An ignored file alone does not count. The rule rides the repo's
    // info/exclude so the rule file itself never dirties the worktree.
    std::fs::create_dir_all(fixture.repo_dir.path().join(".git/info")).unwrap();
    std::fs::write(
        fixture.repo_dir.path().join(".git/info/exclude"),
        "ignored.log\n",
    )
    .unwrap();
    std::fs::write(std::path::Path::new(&path).join("ignored.log"), "noise\n").unwrap();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert!(
        !worktree(&snapshot, "wt-dirty").dirty,
        "ignored files are not dirt"
    );

    // An untracked file does.
    std::fs::write(std::path::Path::new(&path).join("untracked.txt"), "dirt\n").unwrap();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert!(worktree(&snapshot, "wt-dirty").dirty);
}

#[tokio::test]
async fn merged_reads_the_marker_ref_not_ancestry() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();

    // A fresh branch off the trunk tip with no commits: an ancestor of the
    // trunk head, but never merged.
    add_worktree(&repo, &wt_dir, "wt-fresh", "wt-fresh", "main");

    // A branch fast-forwarded into the trunk with the marker ref written:
    // merged, and the merge point is the branch head itself.
    let merged_path = add_worktree(&repo, &wt_dir, "wt-merged", "wt-merged", "main");
    let merged_head = {
        let merged_repo = Repository::open(&merged_path).unwrap();
        common::commit_on(
            &merged_repo,
            "refs/heads/wt-merged",
            Some(main_tip),
            &[("README.md", "hello\n"), ("merged.txt", "merged\n")],
            "merged work",
        )
    };
    // Fast-forward the trunk to the branch head and write the marker ref.
    repo.reference("refs/heads/main", merged_head, true, "ff merge")
        .unwrap();
    repo.reference(
        &format!("refs/holt/merged/{}", "wt-merged"),
        merged_head,
        true,
        "holt: merge wt-merged (ff)",
    )
    .unwrap();
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let fresh = worktree(&snapshot, "wt-fresh");
    assert!(!fresh.merged, "a branch with no commits is not merged");
    assert_eq!(fresh.ahead, 0);
    let merged = worktree(&snapshot, "wt-merged");
    assert!(merged.merged);
    assert_eq!(
        merged.merge_sha.as_deref(),
        Some(merged_head.to_string().as_str())
    );

    // A new commit on the merged branch moves the head past the marker: the
    // worktree stops reading as merged.
    let merged_repo = Repository::open(&merged_path).unwrap();
    let merged_tip = merged_repo
        .find_reference("refs/heads/wt-merged")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    common::commit_on(
        &merged_repo,
        "refs/heads/wt-merged",
        Some(merged_tip),
        &[
            ("README.md", "hello\n"),
            ("merged.txt", "merged\n"),
            ("more.txt", "more\n"),
        ],
        "more work",
    );
    drop(merged_repo);
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let merged = worktree(&snapshot, "wt-merged");
    assert!(!merged.merged, "the head moved past the marker ref");
}

#[tokio::test]
async fn squash_trailer_commit_is_the_merge_point() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let path = add_worktree(&repo, &wt_dir, "wt-sq", "wt-sq", "main");
    let branch_head = {
        let worktree_repo = Repository::open(&path).unwrap();
        common::commit_on(
            &worktree_repo,
            "refs/heads/wt-sq",
            Some(main_tip),
            &[("README.md", "hello\n"), ("sq.txt", "squashed\n")],
            "branch work",
        )
    };
    // A squash-style commit lands on the trunk carrying the trailer; the
    // branch head stays out of the trunk's history.
    let squash_oid = {
        let parent = repo.find_commit(main_tip).unwrap();
        let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        let blob = repo.blob(b"squashed\n").unwrap();
        builder.insert("sq.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = common::signature();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "squash the branch\n\nHolt-Merged-From: wt-sq\n",
            &tree,
            &[&parent],
        )
        .unwrap()
    };
    repo.reference(
        "refs/holt/merged/wt-sq",
        branch_head,
        true,
        "holt: merge wt-sq (squash)",
    )
    .unwrap();
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let worktree = worktree(&snapshot, "wt-sq");
    assert!(worktree.merged);
    assert_eq!(
        worktree.merge_sha.as_deref(),
        Some(squash_oid.to_string().as_str()),
        "the trailer commit on the trunk is the merge point"
    );
}

#[tokio::test]
async fn manual_worktrees_are_listed_and_carry_no_chat() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    add_worktree(&repo, &wt_dir, "manual", "manual-branch", "main");
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let manual = worktree(&snapshot, "manual");
    assert_eq!(manual.branch.as_deref(), Some("manual-branch"));
    assert!(
        manual.chat_id.is_none(),
        "no chat matches the worktree name"
    );
}

#[tokio::test]
async fn chat_id_backfills_from_the_chat_registry() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    add_worktree(&repo, &wt_dir, "chat-1", "branch-of-chat-1", "main");
    drop(repo);

    let engine = fixture.engine();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "chat-1",
                "spaceId": holt_proto::HOME_SPACE_ID,
            }),
        )
        .await
        .unwrap();

    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let row = worktree(&snapshot, "chat-1");
    assert_eq!(row.chat_id.as_deref(), Some("chat-1"));

    // Delete the chat: the backfill stops carrying the id.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert!(
        worktree(&snapshot, "chat-1").chat_id.is_none(),
        "the chat row is gone"
    );
}

#[tokio::test]
async fn a_deleted_directory_reports_missing_without_failing() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let path = add_worktree(&repo, &wt_dir, "wt-gone", "branch-gone", "main");
    drop(repo);
    std::fs::remove_dir_all(&path).unwrap();

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let gone = worktree(&snapshot, "wt-gone");
    assert!(gone.missing);
    assert!(gone.head_sha.is_none());
}

#[tokio::test]
async fn trunk_override_resolves_and_unknown_trunks_fail() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let dev_tip = common::commit_on(
        &repo,
        "refs/heads/dev",
        Some(main_tip),
        &[("README.md", "hello\n"), ("dev.txt", "dev\n")],
        "on dev",
    );
    add_worktree(&repo, &wt_dir, "wt-on-dev", "branch-on-dev", "dev");
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), Some("dev"))
        .await
        .unwrap();
    assert_eq!(snapshot.trunk.name, "dev");
    assert_eq!(snapshot.trunk.head_sha, dev_tip.to_string());
    let worktree = worktree(&snapshot, "wt-on-dev");
    assert_eq!(worktree.behind, 0);

    let error = match list_integration(&engine, &fixture.repo_path(), Some("no-such-trunk")).await {
        Ok(_) => panic!("an unknown trunk must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, RpcError::Failed(ref message) if message.contains("no-such-trunk")));
}

#[tokio::test]
async fn non_git_directories_fail_like_list_refs() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let nowhere = TempDir::new().unwrap();
    let error = match list_integration(&engine, nowhere.path().to_str().unwrap(), None).await {
        Ok(_) => panic!("a non-repo folder must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, RpcError::Failed(_)));
}

#[tokio::test]
async fn trunk_commit_cap_marks_truncation() {
    let fixture = Fixture::new();
    // 205 commits on main: past the 200-commit trunk cap.
    let repo = fixture.repo();
    let mut tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    for number in 0..205 {
        tip = common::commit_on(
            &repo,
            "refs/heads/main",
            Some(tip),
            &[
                ("README.md", "hello\n"),
                ("count.txt", &format!("{number}\n")),
            ],
            &format!("commit {number}"),
        );
    }
    drop(repo);

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert_eq!(snapshot.trunk_commits.len(), 200);
    assert!(snapshot.trunk_truncated);
    assert_eq!(snapshot.trunk_commits[0].sha, tip.to_string());
}

// Unix-only: an unreadable index is a chmod away; Windows hosts would need
// an ACL dance the fixture harness cannot do portably.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_worktree_reports_missing_without_failing_the_call() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let healthy_path = add_worktree(&repo, &wt_dir, "wt-ok", "branch-ok", "main");
    let broken_path = add_worktree(&repo, &wt_dir, "wt-broken", "branch-broken", "main");
    {
        let broken_repo = Repository::open(&broken_path).unwrap();
        common::commit_on(
            &broken_repo,
            "refs/heads/wt-broken",
            Some(main_tip),
            &[("README.md", "hello\n"), ("broken.txt", "work\n")],
            "work in the broken worktree",
        );
    }
    drop(repo);
    // Make the broken worktree's index unreadable: opening the worktree
    // succeeds, the status pass does not.
    let index = fixture
        .repo_dir
        .path()
        .join(".git/worktrees/wt-broken/index");
    let mut permissions = std::fs::metadata(&index).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
    std::fs::set_permissions(&index, permissions).unwrap();

    let engine = fixture.engine();
    let snapshot = list_integration(&engine, &fixture.repo_path(), None).await;

    // Restore before any assertion can strand the mode for TempDir cleanup.
    let mut permissions = std::fs::metadata(&index).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o644);
    std::fs::set_permissions(&index, permissions).unwrap();

    let snapshot = snapshot.expect("one broken worktree must not fail the snapshot");
    let broken = worktree(&snapshot, "wt-broken");
    assert!(broken.missing, "the unreadable worktree reports missing");
    assert!(broken.head_sha.is_none(), "nothing half-filled");
    assert!(broken.commits.is_empty());
    let healthy = worktree(&snapshot, "wt-ok");
    assert!(!healthy.missing);
    assert!(healthy.head_sha.is_some(), "the sibling worktree is whole");
    let _ = healthy_path;
}

// ---- P1: local merge (§7.7) ----

use holt_proto::{MergeHeads, MergeOutcome, MergePreview};

/// A worktree on a fresh branch off the trunk tip, one commit ahead adding
/// `files`. Returns its path.
fn worktree_ahead(
    fixture: &Fixture,
    wt_dir: &TempDir,
    name: &str,
    entries: &[(&str, &str)],
    message: &str,
) -> String {
    let repo = fixture.repo();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let path = add_worktree(&repo, wt_dir, name, &format!("branch-{name}"), "main");
    let worktree_repo = Repository::open(&path).unwrap();
    let mut all: Vec<(String, String)> = repo
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .tree()
        .unwrap()
        .iter()
        .map(|entry| {
            let blob = repo.find_blob(entry.id()).unwrap();
            (
                entry.name().unwrap().to_string(),
                String::from_utf8_lossy(blob.content()).into_owned(),
            )
        })
        .collect();
    drop(worktree_repo);
    drop(repo);
    for (file, content) in entries {
        all.retain(|(existing, _)| existing != file);
        all.push(((*file).to_string(), content.to_string()));
    }
    let sorted: Vec<(&str, &str)> = all
        .iter()
        .map(|(file, content)| (file.as_str(), content.as_str()))
        .collect();
    let repo = fixture.repo();
    {
        let worktree_repo = Repository::open(&path).unwrap();
        common::commit_on(
            &worktree_repo,
            &format!("refs/heads/branch-{name}"),
            Some(tip),
            &sorted,
            message,
        );
        // The commit never touches the workdir: sync it so the worktree is
        // clean at its branch head.
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        worktree_repo.checkout_head(Some(&mut checkout)).unwrap();
    }
    drop(repo);
    path
}

async fn preview_merge_rpc(
    engine: &LocalEngine,
    fixture: &Fixture,
    worktree: &str,
    mode: &str,
) -> Result<MergePreview, RpcError> {
    match engine
        .handle(
            methods::PREVIEW_MERGE,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": worktree,
                "trunk": "main",
                "mode": mode,
            }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("PreviewMerge did not return a value"),
        Err(error) => Err(error),
    }
}

async fn merge_into_trunk_rpc(
    engine: &LocalEngine,
    fixture: &Fixture,
    worktree: &str,
    mode: &str,
    message: Option<&str>,
    heads: &MergeHeads,
) -> Result<holt_proto::MergeResult, RpcError> {
    let mut params = serde_json::json!({
        "repoPath": fixture.repo_path(),
        "worktree": worktree,
        "trunk": "main",
        "mode": mode,
        "expect": heads,
    });
    if let Some(message) = message {
        params["message"] = serde_json::Value::String(message.to_string());
    }
    match engine.handle(methods::MERGE_INTO_TRUNK, params).await {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("MergeIntoTrunk did not return a value"),
        Err(error) => Err(error),
    }
}

fn main_head(fixture: &Fixture) -> git2::Oid {
    fixture
        .repo()
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
}

#[tokio::test]
async fn squash_lands_a_single_parent_commit_with_the_trunk_tree_and_trailer() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let path = worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-sq",
        &[("feature.txt", "feature work\n")],
        "add the feature",
    );
    let engine = fixture.engine();

    let preview = preview_merge_rpc(&engine, &fixture, "wt-sq", "squash")
        .await
        .unwrap();
    let MergeOutcome::Ok {
        files,
        patch,
        message,
        ..
    } = preview.outcome
    else {
        panic!("a clean preview, got {:?}", preview.outcome);
    };
    assert!(files.iter().any(|file| file.path == "feature.txt"));
    assert!(patch.contains("+feature work"));
    assert!(
        message.contains("Squashed 1 commits from branch-wt-sq:"),
        "message: {message}"
    );
    assert!(message.contains("- add the feature"), "message: {message}");
    assert_eq!(
        message.lines().last().unwrap(),
        "Holt-Merged-From: wt-sq",
        "the trailer is the last line"
    );

    let result = merge_into_trunk_rpc(&engine, &fixture, "wt-sq", "squash", None, &preview.heads)
        .await
        .unwrap();
    let repo = fixture.repo();
    let merged = repo
        .find_commit(result.merged_sha.parse().unwrap())
        .unwrap();
    assert_eq!(merged.parent_count(), 1, "a squash has one parent");
    assert_eq!(
        merged.parent_id(0).unwrap().to_string(),
        preview.heads.trunk_head,
        "the only parent is the pre-merge trunk head"
    );
    // The tree is exactly the trunk tree plus the feature file: the
    // independent cross-check `git merge --squash` would produce.
    let blob = merged
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("feature.txt"))
        .unwrap_or_else(|_| panic!("feature.txt in the merged tree"));
    assert_eq!(
        repo.find_blob(blob.id()).unwrap().content(),
        b"feature work\n"
    );
    assert!(
        merged
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("README.md"))
            .is_ok()
    );
    assert_eq!(
        merged.message().ok().unwrap().lines().last().unwrap(),
        "Holt-Merged-From: wt-sq"
    );
    // The branch head never entered the trunk's history.
    let branch_head = Repository::open(&path)
        .unwrap()
        .find_reference("refs/heads/branch-wt-sq")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_ne!(result.merged_sha, branch_head.to_string());
    // The merged marker points at the branch head.
    assert_eq!(
        repo.find_reference("refs/holt/merged/wt-sq")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id(),
        branch_head
    );
    // The main checkout (on the trunk, clean) synced and stays clean.
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("feature.txt")).unwrap(),
        "feature work\n"
    );
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true);
    let statuses = repo.statuses(Some(&mut options)).unwrap();
    assert!(statuses.is_empty(), "main checkout clean after the merge");
}

#[tokio::test]
async fn fast_forward_moves_the_trunk_to_the_branch_head() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let path = worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-ff",
        &[("ff.txt", "ff work\n")],
        "ff commit",
    );
    let engine = fixture.engine();

    let preview = preview_merge_rpc(&engine, &fixture, "wt-ff", "ff")
        .await
        .unwrap();
    assert!(matches!(preview.outcome, MergeOutcome::Ok { .. }));
    let result = merge_into_trunk_rpc(&engine, &fixture, "wt-ff", "ff", None, &preview.heads)
        .await
        .unwrap();
    let branch_head = Repository::open(&path)
        .unwrap()
        .find_reference("refs/heads/branch-wt-ff")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(result.merged_sha, branch_head.to_string());
    assert_eq!(main_head(&fixture), branch_head);
    assert!(
        fixture.repo_dir.path().join("ff.txt").exists(),
        "the main checkout's working tree followed"
    );
}

#[tokio::test]
async fn a_moved_trunk_between_preview_and_merge_refuses_as_stale() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    worktree_ahead(&fixture, &wt_dir, "wt-a", &[("a.txt", "a\n")], "a work");
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-a", "squash")
        .await
        .unwrap();

    // Someone commits to the trunk after the preview.
    let repo = fixture.repo();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    common::commit_on(
        &repo,
        "refs/heads/main",
        Some(tip),
        &[("README.md", "hello\n"), ("trunk.txt", "moved\n")],
        "trunk moves",
    );
    drop(repo);

    let error =
        match merge_into_trunk_rpc(&engine, &fixture, "wt-a", "squash", None, &preview.heads).await
        {
            Ok(_) => panic!("a moved trunk must refuse"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(
        message.starts_with("merge-refused: stale"),
        "stable first line expected, got: {message}"
    );
}

#[tokio::test]
async fn same_line_edits_conflict_the_preview_and_touch_nothing() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // The trunk rewrites README after the branch forked, editing the same
    // line the branch edits.
    let path = add_worktree(&repo, &wt_dir, "wt-conf", "branch-wt-conf", "main");
    {
        let worktree_repo = Repository::open(&path).unwrap();
        common::commit_on(
            &worktree_repo,
            "refs/heads/branch-wt-conf",
            Some(tip),
            &[("README.md", "branch's line\n")],
            "branch edits the line",
        );
    }
    common::commit_on(
        &repo,
        "refs/heads/main",
        Some(tip),
        &[("README.md", "trunk's line\n")],
        "trunk edits the line",
    );
    drop(repo);

    let engine = fixture.engine();
    let before_head = main_head(&fixture);
    let before_status = std::fs::read_to_string(fixture.repo_dir.path().join("README.md")).unwrap();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-conf", "squash")
        .await
        .unwrap();
    let MergeOutcome::Conflict { paths } = preview.outcome else {
        panic!("a both-sides edit must conflict, got {:?}", preview.outcome)
    };
    assert_eq!(paths, vec!["README.md".to_string()]);
    // Nothing changed on disk or in the refs.
    assert_eq!(main_head(&fixture), before_head);
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("README.md")).unwrap(),
        before_status
    );
}

#[tokio::test]
async fn an_overlapping_dirty_main_checkout_refuses_without_touching_anything() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-dirty",
        &[("feature.txt", "feature work\n")],
        "add the feature",
    );
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-dirty", "squash")
        .await
        .unwrap();

    // Uncommitted work on a file the merge also changes.
    std::fs::write(
        fixture.repo_dir.path().join("feature.txt"),
        "precious uncommitted\n",
    )
    .unwrap();
    let before_head = main_head(&fixture);

    let error = match merge_into_trunk_rpc(
        &engine,
        &fixture,
        "wt-dirty",
        "squash",
        None,
        &preview.heads,
    )
    .await
    {
        Ok(_) => panic!("an overlapping dirty checkout must refuse"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.starts_with("merge-refused: trunk-checked-out-dirty"),
        "stable first line expected, got: {message}"
    );
    assert!(
        message.lines().any(|line| line == "feature.txt"),
        "detail lists the overlapping file: {message}"
    );
    // File, index, and trunk ref are byte-for-byte where they were.
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("feature.txt")).unwrap(),
        "precious uncommitted\n"
    );
    assert_eq!(main_head(&fixture), before_head);
    let repo = fixture.repo();
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true);
    let statuses = repo.statuses(Some(&mut options)).unwrap();
    let entries: Vec<String> = statuses
        .iter()
        .filter_map(|entry| entry.path().ok().map(str::to_string))
        .collect();
    assert_eq!(entries, vec!["feature.txt".to_string()], "index untouched");
}

#[tokio::test]
async fn a_non_overlapping_dirty_main_checkout_merges_and_keeps_the_changes() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-keep",
        &[("feature.txt", "feature work\n")],
        "add the feature",
    );
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-keep", "squash")
        .await
        .unwrap();

    std::fs::write(
        fixture.repo_dir.path().join("unrelated.txt"),
        "keep my work\n",
    )
    .unwrap();
    let result = merge_into_trunk_rpc(&engine, &fixture, "wt-keep", "squash", None, &preview.heads)
        .await
        .unwrap();
    assert!(result.main_checkout_synced);
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("feature.txt")).unwrap(),
        "feature work\n",
        "the merged file landed"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("unrelated.txt")).unwrap(),
        "keep my work\n",
        "the uncommitted change survived"
    );
}

#[tokio::test]
async fn a_failed_ref_update_rolls_the_working_tree_back() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-roll",
        &[("feature.txt", "feature work\n")],
        "add the feature",
    );
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-roll", "squash")
        .await
        .unwrap();
    let before_head = main_head(&fixture);

    // Hold the trunk ref's lock file: libgit2's set_target cannot take it.
    let lock = fixture.repo_dir.path().join(".git/refs/heads/main.lock");
    std::fs::write(&lock, b"").unwrap();
    let error =
        match merge_into_trunk_rpc(&engine, &fixture, "wt-roll", "squash", None, &preview.heads)
            .await
        {
            Ok(_) => panic!("a locked ref must fail the merge"),
            Err(error) => error,
        };
    assert!(!error.to_string().contains("merge-refused"));
    std::fs::remove_file(&lock).unwrap();

    assert_eq!(
        main_head(&fixture),
        before_head,
        "the trunk ref never moved"
    );
    assert!(
        !fixture.repo_dir.path().join("feature.txt").exists(),
        "the working tree rolled back to the old trunk"
    );
    let repo = fixture.repo();
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true);
    let statuses = repo.statuses(Some(&mut options)).unwrap();
    assert!(statuses.is_empty(), "the rollback left the tree clean");
}

#[tokio::test]
async fn a_trunk_checked_out_in_a_linked_worktree_refuses() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let repo = fixture.repo();
    worktree_ahead(&fixture, &wt_dir, "wt-else", &[("e.txt", "e\n")], "e work");
    // Move the MAIN checkout off the trunk and check the trunk out in a
    // second linked worktree.
    repo.branch(
        "side",
        &repo.head().unwrap().peel_to_commit().unwrap(),
        false,
    )
    .unwrap();
    repo.checkout_tree(
        repo.find_branch("side", git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel(git2::ObjectType::Tree)
            .unwrap()
            .as_tree()
            .unwrap()
            .as_object(),
        Some(&mut {
            let mut checkout = git2::build::CheckoutBuilder::new();
            checkout.force();
            checkout
        }),
    )
    .unwrap();
    repo.set_head("refs/heads/side").unwrap();
    let trunk_path = add_worktree(&repo, &wt_dir, "wt-holds-trunk", "main", "main");
    drop(repo);

    let engine = fixture.engine();
    let before_head = main_head(&fixture);
    let preview = preview_merge_rpc(&engine, &fixture, "wt-else", "squash")
        .await
        .unwrap();
    let MergeOutcome::Refused { reason, detail } = preview.outcome else {
        panic!(
            "a trunk held elsewhere must refuse, got {:?}",
            preview.outcome
        )
    };
    assert_eq!(reason, holt_proto::MergeRefusal::TrunkCheckedOutElsewhere);
    // The engine reports the addressed path spelling; `canonicalize`'s
    // raw output matches it only on unix.
    let trunk_path = common::addressed(&std::fs::canonicalize(&trunk_path).unwrap());
    assert!(
        detail.first().is_some_and(|line| line == &trunk_path),
        "detail {:?} vs {trunk_path}",
        detail
    );

    let error =
        match merge_into_trunk_rpc(&engine, &fixture, "wt-else", "squash", None, &preview.heads)
            .await
        {
            Ok(_) => panic!("the merge must refuse too"),
            Err(error) => error,
        };
    assert!(
        error
            .to_string()
            .starts_with("merge-refused: trunk-checked-out-elsewhere")
    );
    assert_eq!(main_head(&fixture), before_head, "the ref never moved");
}

#[tokio::test]
async fn the_merge_refusals_cover_empty_impossible_missing_and_detached() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let engine = fixture.engine();

    // ahead == 0: a fresh branch with no commits.
    add_worktree(&fixture.repo(), &wt_dir, "wt-fresh", "branch-fresh", "main");
    let preview = preview_merge_rpc(&engine, &fixture, "wt-fresh", "squash")
        .await
        .unwrap();
    assert_eq!(
        preview.outcome,
        MergeOutcome::Refused {
            reason: holt_proto::MergeRefusal::NothingToMerge,
            detail: Vec::new(),
        }
    );

    // behind > 0 with ff: fork before the trunk moves, then move it.
    let repo = fixture.repo();
    let fork_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let behind_path = add_worktree(&repo, &wt_dir, "wt-behind", "branch-wt-behind", "main");
    {
        let worktree_repo = Repository::open(&behind_path).unwrap();
        common::commit_on(
            &worktree_repo,
            "refs/heads/branch-wt-behind",
            Some(fork_tip),
            &[("README.md", "hello\n"), ("b.txt", "b\n")],
            "b work",
        );
    }
    common::commit_on(
        &repo,
        "refs/heads/main",
        Some(fork_tip),
        &[("README.md", "hello\n"), ("trunk-moved.txt", "m\n")],
        "trunk moves on",
    );
    drop(repo);
    let preview = preview_merge_rpc(&engine, &fixture, "wt-behind", "ff")
        .await
        .unwrap();
    assert_eq!(
        preview.outcome,
        MergeOutcome::Refused {
            reason: holt_proto::MergeRefusal::FfNotPossible,
            detail: Vec::new(),
        }
    );

    // Unknown worktree name.
    let preview = preview_merge_rpc(&engine, &fixture, "no-such-worktree", "squash")
        .await
        .unwrap();
    assert_eq!(
        preview.outcome,
        MergeOutcome::Refused {
            reason: holt_proto::MergeRefusal::WorktreeMissing,
            detail: vec!["no-such-worktree".to_string()],
        }
    );

    // A detached worktree: created on a branch, then set to a detached
    // HEAD (git_worktree_add with no reference mints a branch instead).
    let repo = fixture.repo();
    let detached_path = wt_dir.path().join("wt-det");
    let det_tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    {
        let det_tip = repo.find_commit(det_tip_id).unwrap();
        repo.branch("branch-det", &det_tip, false).unwrap();
    }
    let det_reference = repo.find_reference("refs/heads/branch-det").unwrap();
    let mut det_options = git2::WorktreeAddOptions::new();
    det_options.reference(Some(&det_reference));
    repo.worktree("wt-det", &detached_path, Some(&det_options))
        .unwrap();
    drop(det_reference);
    let det_repo = Repository::open(&detached_path).unwrap();
    det_repo.set_head_detached(det_tip_id).unwrap();
    drop(det_repo);
    drop(repo);
    let preview = preview_merge_rpc(&engine, &fixture, "wt-det", "squash")
        .await
        .unwrap();
    assert_eq!(
        preview.outcome,
        MergeOutcome::Refused {
            reason: holt_proto::MergeRefusal::BranchDetached,
            detail: Vec::new(),
        }
    );
}

#[tokio::test]
async fn merging_flips_the_snapshot_and_a_new_commit_flips_it_back() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let path = worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-flip",
        &[("flip.txt", "f\n")],
        "flip work",
    );
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-flip", "squash")
        .await
        .unwrap();
    let result = merge_into_trunk_rpc(&engine, &fixture, "wt-flip", "squash", None, &preview.heads)
        .await
        .unwrap();

    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    let row = worktree(&snapshot, "wt-flip");
    assert!(row.merged);
    assert_eq!(row.merge_sha.as_deref(), Some(result.merged_sha.as_str()));

    // New work on the branch moves the head past the marker.
    let worktree_repo = Repository::open(&path).unwrap();
    let branch_tip = worktree_repo
        .find_reference("refs/heads/branch-wt-flip")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    common::commit_on(
        &worktree_repo,
        "refs/heads/branch-wt-flip",
        Some(branch_tip),
        &[("flip.txt", "f\n"), ("more.txt", "more\n")],
        "more work",
    );
    drop(worktree_repo);
    let snapshot = list_integration(&engine, &fixture.repo_path(), None)
        .await
        .unwrap();
    assert!(!worktree(&snapshot, "wt-flip").merged);
}

#[tokio::test]
async fn a_user_message_without_the_trailer_gets_it_appended() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-msg",
        &[("msg.txt", "m\n")],
        "msg work",
    );
    let engine = fixture.engine();
    let preview = preview_merge_rpc(&engine, &fixture, "wt-msg", "squash")
        .await
        .unwrap();
    let result = merge_into_trunk_rpc(
        &engine,
        &fixture,
        "wt-msg",
        "squash",
        Some("my own subject\n\nmy own body"),
        &preview.heads,
    )
    .await
    .unwrap();
    let message = fixture
        .repo()
        .find_commit(result.merged_sha.parse().unwrap())
        .unwrap()
        .message()
        .ok()
        .unwrap()
        .to_string();
    assert!(message.starts_with("my own subject\n\nmy own body\n"));
    assert_eq!(message.lines().last().unwrap(), "Holt-Merged-From: wt-msg");
}

// ---- P1: DeleteWorktree (§7.3) ----

async fn delete_worktree(
    engine: &LocalEngine,
    fixture: &Fixture,
    worktree: &str,
    force: bool,
) -> Result<(), RpcError> {
    match engine
        .handle(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": worktree,
                "force": force,
            }),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn spaces_frame(engine: &LocalEngine) -> Vec<holt_proto::Space> {
    let RpcReply::Stream(mut spaces) = engine
        .handle(methods::WATCH_SPACES, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSpaces did not return a stream");
    };
    let value = common::next_frame(&mut spaces).await;
    serde_json::from_value(value).unwrap()
}

async fn chats_frame(engine: &LocalEngine) -> Vec<holt_proto::Chat> {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let value = common::next_frame(&mut chats).await;
    serde_json::from_value(value).unwrap()
}

/// The session-worktree scene: a worktree named after a chat, a Space row
/// for the repo, and a `wt-<chatId>` Space row.
async fn session_scene(fixture: &Fixture, chat_id: &str) -> String {
    let worktrees_root = fixture.data_dir.path().join("worktrees");
    std::fs::create_dir_all(&worktrees_root).unwrap();
    let path = worktrees_root.join(chat_id).display().to_string();
    let repo = fixture.repo();
    {
        let branch_ref = repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        repo.branch(&format!("branch-{chat_id}"), &branch_ref, false)
            .unwrap();
    }
    let mut options = git2::WorktreeAddOptions::new();
    let reference = repo
        .find_reference(&format!("refs/heads/branch-{chat_id}"))
        .unwrap();
    options.reference(Some(&reference));
    repo.worktree(chat_id, &worktrees_root.join(chat_id), Some(&options))
        .unwrap();
    drop(reference);
    drop(repo);
    let engine = fixture.engine();
    for (space_id, space_path) in [
        ("space-main".to_string(), fixture.repo_path()),
        (format!("wt-{chat_id}"), path.clone()),
    ] {
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createSpace",
                    "spaceId": space_id,
                    "deviceId": engine.engine_info().device_id,
                    "path": space_path,
                    "gitDetected": true,
                }),
            )
            .await
            .unwrap();
    }
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": chat_id,
                "spaceId": format!("wt-{chat_id}"),
                "worktree": {
                    "repoPath": fixture.repo_path(),
                    "base": "main",
                },
            }),
        )
        .await
        .unwrap();
    path
}

#[tokio::test]
async fn delete_worktree_removes_a_clean_worktree_and_its_branch() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let path = worktree_ahead(&fixture, &wt_dir, "wt-del", &[("d.txt", "d\n")], "d work");
    let engine = fixture.engine();
    delete_worktree(&engine, &fixture, "wt-del", false)
        .await
        .unwrap();

    assert!(!std::path::Path::new(&path).exists(), "the directory went");
    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(
        !names.contains(&"wt-del".to_string()),
        "registration pruned"
    );
    assert!(
        repo.find_reference("refs/heads/branch-wt-del").is_err(),
        "the branch went"
    );
}

#[tokio::test]
async fn delete_worktree_refuses_dirty_without_force_and_deletes_with_it() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let path = worktree_ahead(
        &fixture,
        &wt_dir,
        "wt-dirty-del",
        &[("x.txt", "x\n")],
        "x work",
    );
    std::fs::write(
        std::path::Path::new(&path).join("scratch.txt"),
        "would be lost\n",
    )
    .unwrap();
    let engine = fixture.engine();

    let error = match delete_worktree(&engine, &fixture, "wt-dirty-del", false).await {
        Ok(()) => panic!("dirty without force must refuse"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.lines().any(|line| line == "scratch.txt"),
        "detail lists what would be lost: {message}"
    );
    assert!(std::path::Path::new(&path).exists(), "nothing removed");

    delete_worktree(&engine, &fixture, "wt-dirty-del", true)
        .await
        .unwrap();
    assert!(!std::path::Path::new(&path).exists());
}

#[tokio::test]
async fn delete_worktree_retires_the_session_chat_and_space() {
    let fixture = Fixture::new();
    let path = session_scene(&fixture, "chat-7").await;
    let engine = fixture.engine();
    delete_worktree(&engine, &fixture, "chat-7", false)
        .await
        .unwrap();

    assert!(!std::path::Path::new(&path).exists());
    let chats = chats_frame(&engine).await;
    let chat = chats.iter().find(|chat| chat.id == "chat-7").unwrap();
    assert!(chat.archived, "the chat is archived, not deleted");
    assert_eq!(
        chat.space_id.as_deref(),
        Some("space-main"),
        "re-homed onto the repo's main Space"
    );
    let spaces = spaces_frame(&engine).await;
    assert!(spaces.iter().any(|space| space.id == "space-main"));
    assert!(
        !spaces.iter().any(|space| space.id == "wt-chat-7"),
        "the session Space row went"
    );
    let repo = fixture.repo();
    assert!(
        repo.find_reference(&format!("refs/heads/{}", "branch-chat-7"))
            .is_err(),
        "the session branch went"
    );
}

#[tokio::test]
async fn delete_worktree_refuses_unregistered_names() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let error = match delete_worktree(&engine, &fixture, "no-such-worktree", false).await {
        Ok(()) => panic!("an unknown worktree must refuse"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("not registered"), "got: {error}");
}

#[tokio::test]
async fn a_message_to_the_retired_chat_rematerializes_the_worktree() {
    let fixture = Fixture::new();
    let path = session_scene(&fixture, "chat-8").await;
    let engine = fixture.engine();
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    delete_worktree(&engine, &fixture, "chat-8", false)
        .await
        .unwrap();

    // The user sends a message to the archived chat: the Turn runs in a
    // FRESH worktree materialized from the chat's spec.
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-8",
                "command": {
                    "kind": "run",
                    "messageId": "m-1",
                    "request": {
                        "prompt": "continue the work",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "cwd": fixture.repo_path(),
                    },
                },
            }),
        )
        .await
        .unwrap();
    common::wait_for_chat_status(&engine, "chat-8", "idle").await;

    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(
        names.contains(&"chat-8".to_string()),
        "the worktree rematerialized, got {names:?}"
    );
    assert!(
        std::path::Path::new(&path).join("README.md").exists(),
        "a checked-out worktree, not a stub"
    );
    let chats = chats_frame(&engine).await;
    let chat = chats.iter().find(|chat| chat.id == "chat-8").unwrap();
    assert!(!chat.archived, "the send revived the archived chat");
    assert_eq!(
        chat.space_id.as_deref(),
        Some("wt-chat-8"),
        "re-parented onto the rebuilt session Space"
    );
    let spaces = spaces_frame(&engine).await;
    assert!(
        spaces.iter().any(|space| space.id == "wt-chat-8"),
        "the session Space was rebuilt"
    );
}

#[tokio::test]
async fn delete_worktree_refuses_a_chat_with_a_running_turn() {
    let fixture = Fixture::new();
    session_scene(&fixture, "chat-9").await;
    let engine = fixture.silent_engine();
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-9",
                "command": {
                    "kind": "run",
                    "messageId": "m-1",
                    "request": {
                        "prompt": "hold the turn open",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "cwd": fixture.repo_path(),
                    },
                },
            }),
        )
        .await
        .unwrap();
    common::wait_for_chat_status(&engine, "chat-9", "working").await;

    let error = match delete_worktree(&engine, &fixture, "chat-9", true).await {
        Ok(()) => panic!("a running Turn must refuse the delete"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stop it first"), "got: {error}");
    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(names.contains(&"chat-9".to_string()), "nothing removed");
}

#[tokio::test]
async fn delete_worktree_refuses_a_worktree_holding_the_trunk() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    // The main checkout moves to a side branch so the trunk (main) is free
    // for a linked worktree to check out.
    let repo = fixture.repo();
    let tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    {
        let tip = repo.find_commit(tip_id).unwrap();
        repo.branch("side", &tip, false).unwrap();
    }
    repo.set_head("refs/heads/side").unwrap();
    let path = add_worktree(&repo, &wt_dir, "wt-holds-trunk", "main", "main");
    drop(repo);

    let engine = fixture.engine();
    let error = match delete_worktree(&engine, &fixture, "wt-holds-trunk", false).await {
        Ok(()) => panic!("a worktree holding the trunk must refuse"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("the worktree stays"),
        "got: {error}"
    );
    // The whole deletion refused: directory, registration, and branch.
    assert!(std::path::Path::new(&path).exists());
    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(names.contains(&"wt-holds-trunk".to_string()));
    assert!(repo.find_reference("refs/heads/main").is_ok());
}

#[tokio::test]
async fn delete_refuses_the_default_trunk_even_under_a_trunk_override() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    // The page works on a `develop` trunk, but the worktree holds `main` —
    // the repository's own default. Deleting it would strand the repo all
    // the same, so the override must extend the refusal set, not replace it.
    let repo = fixture.repo();
    let tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    {
        let tip = repo.find_commit(tip_id).unwrap();
        repo.branch("side", &tip, false).unwrap();
        repo.branch("develop", &tip, false).unwrap();
    }
    repo.set_head("refs/heads/side").unwrap();
    let path = add_worktree(&repo, &wt_dir, "wt-holds-default", "main", "main");
    drop(repo);

    let engine = fixture.engine();
    let error = match engine
        .handle(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-holds-default",
                "force": false,
                "trunk": "develop",
            }),
        )
        .await
    {
        Ok(_) => panic!("a worktree holding the default branch must refuse"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("the worktree stays"),
        "got: {error}"
    );
    assert!(std::path::Path::new(&path).exists());

    // The mirror case: a worktree holding the OVERRIDE trunk refuses too,
    // even though the repository's default is `main`.
    let repo = fixture.repo();
    let override_path = add_worktree(&repo, &wt_dir, "wt-holds-override", "develop", "develop");
    drop(repo);
    let error = match engine
        .handle(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-holds-override",
                "force": false,
                "trunk": "develop",
            }),
        )
        .await
    {
        Ok(_) => panic!("a worktree holding the override trunk must refuse"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("the worktree stays"),
        "got: {error}"
    );
    assert!(std::path::Path::new(&override_path).exists());
}
