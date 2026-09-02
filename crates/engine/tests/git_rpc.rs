//! Handle-seam tests for the git capability: a real engine assembled on a
//! temp data dir, a real fixture repository built with git2 in another temp
//! dir, driven through the `RpcService` trait exactly as the UI drives it.
//! The fixture repos have no remotes unless a test adds one — branch listing
//! and switching must work fully offline.

use git2::Repository;
use holt_engine::{EngineConfig, StubEngine};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

struct Fixture {
    /// Where the repository's working tree lives.
    repo_dir: TempDir,
    /// The engine's own data dir.
    data_dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let base = commit_on(
            &repo,
            "refs/heads/main",
            None,
            &[("README.md", "hello\n")],
            "initial",
        );
        // Sibling branches ahead of main, each adding its own file. Built
        // through treebuilders so the working tree stays clean on main.
        commit_on(
            &repo,
            "refs/heads/feature",
            Some(base),
            &[("README.md", "hello\n"), ("feature.txt", "feature work\n")],
            "feature",
        );
        commit_on(
            &repo,
            "refs/heads/alpha",
            Some(base),
            &[("README.md", "hello\n"), ("alpha.txt", "a\n")],
            "alpha",
        );
        // Materialize main into the working tree so the fixture starts clean.
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

    fn engine(&self) -> StubEngine {
        StubEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
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

/// Parse the wire shape the picker parses: `[{name, current, worktreePath}]`.
fn refs(value: serde_json::Value) -> Vec<(String, bool, Option<String>)> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_string(),
                entry["current"].as_bool().unwrap_or(false),
                entry["worktreePath"].as_str().map(str::to_string),
            )
        })
        .collect()
}

async fn list_refs(engine: &StubEngine, repo_path: &str) -> Vec<(String, bool, Option<String>)> {
    let RpcReply::Value(value) = engine
        .handle(
            methods::LIST_REFS,
            serde_json::json!({ "repoPath": repo_path }),
        )
        .await
        .unwrap()
    else {
        panic!("ListRefs did not return a value");
    };
    refs(value)
}

#[tokio::test]
async fn list_refs_orders_default_first_current_tagged_offline() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let listed = list_refs(&engine, &fixture.repo_path()).await;

    // No remotes on this repo: main wins by name, the rest alphabetical,
    // current tagged on main only.
    let names: Vec<&str> = listed.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["main", "alpha", "feature"]);
    assert_eq!(listed[0], ("main".into(), true, None));
    let current_count = listed.iter().filter(|(_, current, _)| *current).count();
    assert_eq!(
        current_count, 1,
        "exactly the checked-out branch is current"
    );
}

#[tokio::test]
async fn list_refs_resolves_default_from_origin_head() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // A local "trunk" branch plus an origin remote whose HEAD targets it —
    // remote refs must not leak into the list, but trunk becomes default.
    let tip = repo.find_commit(tip_id).unwrap();
    repo.branch("trunk", &tip, false).unwrap();
    repo.reference("refs/remotes/origin/trunk", tip_id, true, "test")
        .unwrap();
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
        true,
        "test",
    )
    .unwrap();
    drop(tip);
    drop(repo);

    let engine = fixture.engine();
    let listed = list_refs(&engine, &fixture.repo_path()).await;
    let names: Vec<&str> = listed.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["trunk", "alpha", "feature", "main"]);
    assert!(
        !names.contains(&"origin/trunk"),
        "local branches only, remote refs must not be listed"
    );
}

#[tokio::test]
async fn list_refs_falls_back_to_alphabetical_without_default_name() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // Rename away every default-shaped branch: only beta/zeta remain, no
    // origin — alphabetical first wins.
    let tip = repo.find_commit(tip_id).unwrap();
    repo.branch("beta", &tip, false).unwrap();
    repo.branch("zeta", &tip, false).unwrap();
    for branch in ["main", "feature", "alpha"] {
        repo.find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .delete()
            .unwrap();
    }
    drop(tip);
    drop(repo);

    let engine = fixture.engine();
    let listed = list_refs(&engine, &fixture.repo_path()).await;
    let names: Vec<&str> = listed.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["beta", "zeta"]);
}

#[tokio::test]
async fn list_refs_tags_linked_worktree_branches_with_their_path() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    // libgit2 creates the worktree directory itself; hand it a fresh child.
    let wt_path = wt_dir.path().join("wt");
    let repo = fixture.repo();
    let feature_ref = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .into_reference();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&feature_ref));
    repo.worktree("wt-feature", &wt_path, Some(&options))
        .unwrap();
    drop(feature_ref);
    drop(repo);

    let engine = fixture.engine();
    let listed = list_refs(&engine, &fixture.repo_path()).await;
    let feature = listed
        .iter()
        .find(|(name, _, _)| name == "feature")
        .unwrap();
    let worktree_path = feature.2.as_deref().expect("worktreePath set");
    assert!(
        std::path::Path::new(worktree_path).is_dir(),
        "worktreePath points at the linked worktree, got {worktree_path}"
    );
    assert!(!feature.1, "feature is checked out elsewhere, not here");
    assert_eq!(listed[0].0, "main");
    assert!(listed[0].1, "main stays the current branch");
}

#[tokio::test]
async fn list_branches_returns_names_with_default_first() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let RpcReply::Value(value) = engine
        .handle(
            methods::LIST_BRANCHES,
            serde_json::json!({ "repoPath": fixture.repo_path() }),
        )
        .await
        .unwrap()
    else {
        panic!("ListBranches did not return a value");
    };
    let names: Vec<String> = serde_json::from_value(value).unwrap();
    assert_eq!(names, ["main", "alpha", "feature"]);
}

#[tokio::test]
async fn switch_ref_checks_out_the_branch_on_a_clean_tree() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "feature",
            }),
        )
        .await
        .unwrap();

    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature");
    assert!(
        fixture.repo_dir.path().join("feature.txt").exists(),
        "the working tree moved to feature"
    );
}

#[tokio::test]
async fn switch_ref_refuses_to_clobber_uncommitted_changes() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    // An untracked file that also exists on the target branch with different
    // content: a safe checkout would overwrite it, so git must refuse.
    std::fs::write(
        fixture.repo_dir.path().join("feature.txt"),
        "precious uncommitted work\n",
    )
    .unwrap();

    let error = match engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "feature",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("dirty switch must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)));
    let message = error.to_string();
    assert!(
        message.to_lowercase().contains("overwritten")
            || message.to_lowercase().contains("conflict"),
        "git's own refusal message expected, got: {message}"
    );

    // The refusal left the tree and HEAD where they were.
    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("feature.txt")).unwrap(),
        "precious uncommitted work\n"
    );
}

#[tokio::test]
async fn create_branch_creates_and_switches_from_head() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    engine
        .handle(
            methods::CREATE_BRANCH,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "name": "fresh",
            }),
        )
        .await
        .unwrap();

    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "fresh");
    // Default base = HEAD: the fresh branch points at main's tip.
    let fresh = repo
        .find_reference("refs/heads/fresh")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let main = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(fresh.id(), main.id());
    // ListRefs now reports the created branch, tagged current (ordering is
    // default-branch first, not current first).
    let listed = list_refs(&engine, &fixture.repo_path()).await;
    let fresh = listed
        .iter()
        .find(|(name, _, _)| name == "fresh")
        .expect("created branch listed");
    assert!(fresh.1, "the created branch is checked out");
    assert!(
        listed.iter().filter(|(_, current, _)| *current).count() == 1,
        "exactly one current branch"
    );
}

#[tokio::test]
async fn create_branch_honors_an_explicit_base_ref() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    engine
        .handle(
            methods::CREATE_BRANCH,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "name": "from-feature",
                "baseRef": "feature",
            }),
        )
        .await
        .unwrap();

    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "from-feature");
    assert!(
        fixture.repo_dir.path().join("feature.txt").exists(),
        "the working tree carries the base branch's file"
    );
    let created = repo
        .find_reference("refs/heads/from-feature")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let feature = repo
        .find_reference("refs/heads/feature")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(created.id(), feature.id());
}

#[tokio::test]
async fn create_branch_rejects_invalid_and_duplicate_names() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    // Invalid ref name: git's own validation message, nothing changes.
    let error = match engine
        .handle(
            methods::CREATE_BRANCH,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "name": "not a valid ref..name",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an invalid branch name must be rejected"),
    };
    assert!(matches!(error, RpcError::Failed(ref message) if !message.is_empty()));
    let repo = fixture.repo();
    assert!(
        repo.find_reference("refs/heads/not a valid ref..name")
            .is_err()
    );

    // Duplicate: rejected rather than silently switching to the existing one.
    let error = match engine
        .handle(
            methods::CREATE_BRANCH,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "name": "feature",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an existing branch name must be rejected"),
    };
    assert!(matches!(error, RpcError::Failed(_)));
    assert!(
        error.to_string().to_lowercase().contains("exists"),
        "git's duplicate message expected, got: {error}"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
}

#[tokio::test]
async fn create_branch_refusal_rolls_the_fresh_branch_back() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    // Precious uncommitted work the create's checkout would clobber.
    std::fs::write(
        fixture.repo_dir.path().join("feature.txt"),
        "precious uncommitted work\n",
    )
    .unwrap();

    let error = match engine
        .handle(
            methods::CREATE_BRANCH,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "name": "fresh",
                "baseRef": "feature",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a clobbering create must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)));

    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
    assert!(
        repo.find_reference("refs/heads/fresh").is_err(),
        "the refused create must not leave the branch behind"
    );
}

#[tokio::test]
async fn switch_ref_unknown_branch_fails_with_gits_message() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let error = match engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "no-such-branch",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("switch to a missing branch must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)));
    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
}

#[tokio::test]
async fn list_refs_requires_a_repo_path_param() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let error = match engine
        .handle(methods::LIST_REFS, serde_json::json!({}))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("missing repoPath must fail"),
    };
    assert!(matches!(error, RpcError::BadParams(_)));
}

#[tokio::test]
async fn list_refs_on_a_non_repo_reports_gits_message() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let nowhere = TempDir::new().unwrap();
    let error = match engine
        .handle(
            methods::LIST_REFS,
            serde_json::json!({ "repoPath": nowhere.path() }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a non-repo folder must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)));
    assert!(error.to_string().to_lowercase().contains("repository"));
}
