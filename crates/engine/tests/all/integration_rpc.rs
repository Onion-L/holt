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
