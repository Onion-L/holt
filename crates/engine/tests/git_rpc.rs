//! Handle-seam tests for the git capability: a real engine assembled on a
//! temp data dir, a real fixture repository built with git2 in another temp
//! dir, driven through the `RpcService` trait exactly as the UI drives it.
//! The fixture repos have no remotes unless a test adds one — branch listing
//! and switching must work fully offline.

use futures::StreamExt as _;
use git2::Repository;
use holt_engine::{EngineConfig, StubEngine};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

/// Body of the fixture's movable file — large enough for git's rename
/// similarity signatures to pair a rename reliably (tiny files fall under
/// the detection threshold).
fn movable_body() -> String {
    let mut body = String::new();
    for line in 0..50 {
        body.push_str(&format!("shared movable content line {line}\n"));
    }
    body
}

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
        let movable: &'static str = Box::leak(movable_body().into_boxed_str());
        let base = commit_on(
            &repo,
            "refs/heads/main",
            None,
            &[("README.md", "hello\n"), ("movable.txt", movable)],
            "initial",
        );
        // Sibling branches ahead of main, each adding its own file. Built
        // through treebuilders so the working tree stays clean on main.
        commit_on(
            &repo,
            "refs/heads/feature",
            Some(base),
            &[
                ("README.md", "hello\n"),
                ("movable.txt", movable),
                ("feature.txt", "feature work\n"),
            ],
            "feature",
        );
        commit_on(
            &repo,
            "refs/heads/alpha",
            Some(base),
            &[
                ("README.md", "hello\n"),
                ("movable.txt", movable),
                ("alpha.txt", "a\n"),
            ],
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

// ---- checkout identity + working-tree diffs (git-capability issue 03) ----

use holt_proto::{CheckoutDiff, GetCheckoutFileDiffTextRequest, Space};

async fn register_space(engine: &StubEngine, fixture: &Fixture, space_id: &str) {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace",
                "spaceId": space_id,
                "deviceId": engine.engine_info().device_id,
                "path": fixture.repo_path(),
                "gitDetected": true,
            }),
        )
        .await
        .unwrap();
}

async fn first_space(engine: &StubEngine) -> Space {
    let RpcReply::Stream(mut spaces) = engine
        .handle(methods::WATCH_SPACES, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchSpaces did not return a stream");
    };
    let value = spaces.next().await.expect("spaces snapshot");
    let spaces: Vec<Space> = serde_json::from_value(value).unwrap();
    spaces.into_iter().next().expect("one registered space")
}

async fn working_tree_diff(engine: &StubEngine, cwd: &str) -> CheckoutDiff {
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_DIFF,
            serde_json::json!({
                "cwd": cwd,
                "mode": "workingTree",
                "chatId": "chat-1",
            }),
        )
        .await
        .unwrap()
    else {
        panic!("GetCheckoutDiff did not return a value");
    };
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn create_space_mints_identity_and_chat_inherits_it() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    let space = first_space(&engine).await;
    let checkout_id = space.checkout_id.expect("checkout identity minted");
    assert_eq!(checkout_id.len(), 64, "sha256 hex");
    assert!(checkout_id.chars().all(|c| c.is_ascii_hexdigit()));

    // Chats inherit the space's identity as they already do.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "chat-1",
                "spaceId": "space-1",
            }),
        )
        .await
        .unwrap();
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!();
    };
    let value = chats.next().await.unwrap();
    let chat: holt_proto::Chat =
        serde_json::from_value(value.as_array().unwrap()[0].clone()).unwrap();
    assert_eq!(chat.checkout_id.as_deref(), Some(checkout_id.as_str()));
}

#[tokio::test]
async fn worktree_space_mints_a_distinct_identity() {
    let fixture = Fixture::new();
    let wt_dir = TempDir::new().unwrap();
    let wt_path = wt_dir.path().join("wt");
    let repo = fixture.repo();
    let feature_ref = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .into_reference();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&feature_ref));
    repo.worktree("wt-identity", &wt_path, Some(&options))
        .unwrap();
    drop(feature_ref);
    drop(repo);

    let engine = fixture.engine();
    // Same repo, two checkouts (main + linked worktree): the spaces never
    // cross-contaminate because the identities differ.
    for (space_id, path) in [
        ("space-main", fixture.repo_path()),
        ("space-wt", wt_path.display().to_string()),
    ] {
        engine
            .handle(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createSpace",
                    "spaceId": space_id,
                    "deviceId": engine.engine_info().device_id,
                    "path": path,
                    "gitDetected": true,
                }),
            )
            .await
            .unwrap();
    }
    let RpcReply::Stream(mut spaces) = engine
        .handle(methods::WATCH_SPACES, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!();
    };
    let value = spaces.next().await.unwrap();
    let spaces: Vec<Space> = serde_json::from_value(value).unwrap();
    let ids: Vec<&str> = spaces
        .iter()
        .map(|s| s.checkout_id.as_deref().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "worktree checkout identity must differ");
}

#[tokio::test]
async fn persisted_spaces_are_backfilled_with_identity_on_load() {
    let fixture = Fixture::new();
    // A pre-feature row: git-detected, no checkout identity.
    std::fs::write(
        fixture.data_dir.path().join("spaces.json"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "id": "space-old",
            "deviceId": "device-old",
            "path": fixture.repo_path(),
            "gitDetected": true,
            "createdAt": "2026-01-01T00:00:00Z",
        }]))
        .unwrap(),
    )
    .unwrap();
    let engine = fixture.engine();
    let space = first_space(&engine).await;
    let backfilled = space.checkout_id.expect("identity backfilled on load");
    assert_eq!(backfilled.len(), 64);
}

#[tokio::test]
async fn watch_emits_snapshot_then_live_frames_and_rekeys_on_commit() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    let RpcReply::Stream(mut items) = engine
        .handle(methods::WATCH_CHECKOUT_DIFFS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchCheckoutDiffs did not return a stream");
    };

    // Opening snapshot: a full list; the fixture tree is clean.
    let first = tokio::time::timeout(std::time::Duration::from_secs(15), items.next())
        .await
        .expect("snapshot within timeout")
        .expect("snapshot present");
    let list: Vec<CheckoutDiff> = serde_json::from_value(first).unwrap();
    assert_eq!(list.len(), 1, "one watched checkout");
    assert!(list[0].patch.trim().is_empty());
    assert!(list[0].files.is_empty());
    assert_eq!(
        list[0].cwd,
        std::fs::canonicalize(fixture.repo_dir.path())
            .unwrap()
            .display()
            .to_string(),
        "the frame's cwd is the canonical checkout root"
    );
    let clean_checksum = list[0].checksum.clone();

    // Let the watch loop finish seeding before mutating.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // A working-tree edit lands as a frame within a beat.
    std::fs::write(fixture.repo_dir.path().join("README.md"), "dirty\n").unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(15), items.next())
        .await
        .expect("edit frame within timeout")
        .expect("frame present");
    let dirty: CheckoutDiff = serde_json::from_value(frame).unwrap();
    assert!(dirty.patch.contains("+dirty"), "patch: {}", dirty.patch);
    assert_ne!(dirty.checksum, clean_checksum);

    // Committing the edit cleans the tree but re-keys via HEAD.
    let repo = fixture.repo();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let sig = signature();
    {
        let tree = repo.find_tree(tree_id).unwrap();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "commit the edit",
            &tree,
            &[&head_commit],
        )
        .unwrap();
    }
    drop(repo);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(15), items.next())
        .await
        .expect("commit frame within timeout")
        .expect("frame present");
    let committed: CheckoutDiff = serde_json::from_value(frame).unwrap();
    assert!(
        committed.patch.trim().is_empty(),
        "tree is clean after the commit"
    );
    assert_ne!(
        committed.checksum, dirty.checksum,
        "the HEAD component re-keys the capture"
    );
    assert_ne!(committed.checksum, clean_checksum);
}

#[tokio::test]
async fn get_checkout_diff_working_tree_matches_the_watch_frame() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    std::fs::write(fixture.repo_dir.path().join("README.md"), "via rpc\n").unwrap();

    let RpcReply::Stream(mut items) = engine
        .handle(methods::WATCH_CHECKOUT_DIFFS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!();
    };
    let frame = tokio::time::timeout(std::time::Duration::from_secs(15), items.next())
        .await
        .expect("snapshot within timeout")
        .unwrap();
    // The snapshot carries the dirty state (recomputed at subscribe).
    let list: Vec<CheckoutDiff> = serde_json::from_value(frame).unwrap();
    let watched = &list[0];

    let served = working_tree_diff(&engine, &fixture.repo_path()).await;
    assert_eq!(served.checksum, watched.checksum);
    assert_eq!(served.patch, watched.patch);
    assert_eq!(served.files, watched.files);
    assert_eq!(served.additions, watched.additions);
    assert_eq!(served.deletions, watched.deletions);
    assert_eq!(served.checkout_id, watched.checkout_id);
}

#[tokio::test]
async fn working_tree_diff_reports_renames_binary_and_counts() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // Unstaged modification.
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "hello modified\n",
    )
    .unwrap();
    // Staged addition.
    std::fs::write(fixture.repo_dir.path().join("staged.txt"), "staged line\n").unwrap();
    let repo = fixture.repo();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("staged.txt")).unwrap();
    index.write().unwrap();
    drop(repo);
    // Rename with a content tweak (similarity high enough for git's
    // rename detection to pair the delete with the untracked add).
    std::fs::rename(
        fixture.repo_dir.path().join("movable.txt"),
        fixture.repo_dir.path().join("moved.txt"),
    )
    .unwrap();
    let mut moved = movable_body();
    moved.push_str("plus a rename tweak\n");
    std::fs::write(fixture.repo_dir.path().join("moved.txt"), moved).unwrap();
    // Untracked binary.
    std::fs::write(
        fixture.repo_dir.path().join("blob.bin"),
        [0u8, 1, 0, 0, 255, 0, 7],
    )
    .unwrap();

    let diff = working_tree_diff(&engine, &fixture.repo_path()).await;
    let summary = |path: &str| {
        diff.files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} missing from {:#?}", diff.files))
    };
    let readme = summary("README.md");
    assert_eq!(readme.status, "modified");
    assert_eq!(readme.additions, 1);
    assert_eq!(readme.deletions, 1);
    let staged = summary("staged.txt");
    assert_eq!(staged.status, "added");
    assert_eq!(staged.additions, 1);
    let moved = summary("moved.txt");
    assert_eq!(moved.status, "renamed");
    assert_eq!(moved.old_path.as_deref(), Some("movable.txt"));
    let blob = summary("blob.bin");
    assert_eq!(blob.status, "added");
    assert!(blob.binary, "binary flagged without a text dump");

    assert!(diff.patch.contains("rename from movable.txt"));
    assert!(
        !diff.patch.contains("blob.bin\u{0}"),
        "no binary bytes dumped"
    );
    assert_eq!(
        diff.additions,
        diff.files.iter().map(|f| f.additions).sum::<u32>()
    );
    assert_eq!(
        diff.deletions,
        diff.files.iter().map(|f| f.deletions).sum::<u32>()
    );
    assert!(!diff.truncated);
}

#[tokio::test]
async fn huge_patch_truncates_but_file_summaries_stay_complete() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    // ~6 MiB of unique added lines: past the 3 MiB patch cap.
    let mut big = String::with_capacity(6 * 1024 * 1024 + 16);
    for line in 0..600_000 {
        big.push_str(&format!("big line {line}\n"));
    }
    std::fs::write(fixture.repo_dir.path().join("big.txt"), &big).unwrap();

    let diff = working_tree_diff(&engine, &fixture.repo_path()).await;
    assert!(diff.truncated, "past the 3 MiB cap");
    assert!(diff.patch.len() < 5 * 1024 * 1024, "patch stays bounded");
    // The summary list is complete even though the patch text is cut.
    assert!(diff.files.iter().any(|file| file.path == "big.txt"));
    let big_file = diff
        .files
        .iter()
        .find(|file| file.path == "big.txt")
        .unwrap();
    assert_eq!(big_file.additions, 600_000);
}

#[tokio::test]
async fn file_diff_text_serves_old_new_sides_binary_and_staleness() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    std::fs::write(fixture.repo_dir.path().join("README.md"), "edited\n").unwrap();
    let current = working_tree_diff(&engine, &fixture.repo_path()).await;

    let request = |checksum: String, path: &str| GetCheckoutFileDiffTextRequest {
        checkout_id: current.checkout_id.clone(),
        cwd: fixture.repo_path(),
        path: path.to_string(),
        mode: "workingTree".into(),
        base_ref: None,
        chat_id: Some("chat-1".into()),
        commit_sha: None,
        diff_checksum: checksum,
    };

    // Fresh request: old from the HEAD blob, new from the workdir file.
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::to_value(request(current.checksum.clone(), "README.md")).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!();
    };
    let text: holt_proto::CheckoutFileDiffText = serde_json::from_value(value).unwrap();
    assert_eq!(text.old_text.as_deref(), Some("hello\n"));
    assert_eq!(text.new_text.as_deref(), Some("edited\n"));
    assert!(!text.stale);
    assert!(!text.binary);
    assert!(!text.truncated);
    assert!(text.old_content_hash.is_some());

    // A newer edit makes the pinned capture stale.
    std::fs::write(fixture.repo_dir.path().join("README.md"), "edited again\n").unwrap();
    let newer = working_tree_diff(&engine, &fixture.repo_path()).await;
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::to_value(request(newer.checksum, "README.md")).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!();
    };
    let text: holt_proto::CheckoutFileDiffText = serde_json::from_value(value).unwrap();
    assert!(!text.stale, "pinned to the fresh capture");

    // Binary content: flagged, no text.
    std::fs::write(fixture.repo_dir.path().join("data.bin"), [1u8, 0, 2, 0]).unwrap();
    let binary_state = working_tree_diff(&engine, &fixture.repo_path()).await;
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::to_value(request(binary_state.checksum, "data.bin")).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!();
    };
    let text: holt_proto::CheckoutFileDiffText = serde_json::from_value(value).unwrap();
    assert!(text.binary);
    assert!(text.old_text.is_none());
    assert!(text.new_text.is_none());
    assert!(text.new_content_hash.is_some());

    // A side past 1 MiB truncates with the flag set.
    let huge = "x".repeat(2 * 1024 * 1024);
    std::fs::write(fixture.repo_dir.path().join("huge.txt"), &huge).unwrap();
    let huge_state = working_tree_diff(&engine, &fixture.repo_path()).await;
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::to_value(request(huge_state.checksum, "huge.txt")).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!();
    };
    let text: holt_proto::CheckoutFileDiffText = serde_json::from_value(value).unwrap();
    assert!(text.truncated, "2 MiB side truncates at 1 MiB");
    assert!(text.new_text.as_deref().unwrap().len() <= 1024 * 1024);
}

// ---- branch scope: merge-base diffs (git-capability issue 04) ----

async fn branch_diff(engine: &StubEngine, cwd: &str, base: &str) -> Result<CheckoutDiff, RpcError> {
    match engine
        .handle(
            methods::GET_CHECKOUT_DIFF,
            serde_json::json!({
                "cwd": cwd,
                "mode": "branch",
                "baseRef": base,
                "chatId": "chat-1",
            }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("GetCheckoutDiff did not return a value"),
        Err(error) => Err(error),
    }
}

/// Commit the current index onto `refs/heads/main` (HEAD) with a message.
fn commit_workdir(fixture: &Fixture, message: &str) {
    let repo = fixture.repo();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let sig = signature();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&head_commit])
        .unwrap();
}

#[tokio::test]
async fn branch_scope_shows_committed_and_uncommitted_work_over_the_base() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // `feature` is one commit ahead of the shared base; move main AHEAD of
    // feature's base too so merge-base(feature, main) = the shared base.
    // The fixture: main is at `initial`; branch `feature` adds feature.txt.
    // Commit on main: a new file plus a README change, then leave one more
    // file uncommitted.
    std::fs::write(
        fixture.repo_dir.path().join("committed.txt"),
        "committed on main\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "committed and dirty\n",
    )
    .unwrap();
    commit_workdir(&fixture, "ahead of base");
    std::fs::write(
        fixture.repo_dir.path().join("uncommitted.txt"),
        "dirty on top\n",
    )
    .unwrap();

    // "What would this branch ship": the CURRENT branch's committed work
    // (committed.txt, README) over the merge-base with `feature`, plus the
    // uncommitted file on top.
    let diff = branch_diff(&engine, &fixture.repo_path(), "feature")
        .await
        .unwrap();
    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"committed.txt"), "paths: {paths:?}");
    assert!(paths.contains(&"uncommitted.txt"), "paths: {paths:?}");
    assert!(paths.contains(&"README.md"), "paths: {paths:?}");
    assert!(diff.patch.contains("+committed on main"));
    assert!(diff.patch.contains("+dirty on top"));
}

#[tokio::test]
async fn branch_scope_with_the_current_branch_as_base_matches_working_tree_content() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    std::fs::write(fixture.repo_dir.path().join("README.md"), "just dirty\n").unwrap();

    // base == the current branch: merge-base(main, HEAD) = HEAD, so the
    // branch diff degenerates to the working-tree diff (same content; the
    // checksum still differs — mode and baseRef fold in).
    let branch = branch_diff(&engine, &fixture.repo_path(), "main")
        .await
        .unwrap();
    let working = working_tree_diff(&engine, &fixture.repo_path()).await;
    assert_eq!(branch.patch, working.patch);
    assert_eq!(branch.files, working.files);
    assert_eq!(branch.additions, working.additions);
    assert_ne!(branch.checksum, working.checksum);
}

#[tokio::test]
async fn branch_scope_re_keys_when_the_base_ref_changes() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // One commit ahead of the shared base, plus a dirty file: a base at the
    // merge-base (feature) sees committed + dirty; the current branch as
    // base degenerates to the dirty file alone. Either way the checksum
    // re-keys with the base ref.
    std::fs::write(
        fixture.repo_dir.path().join("ahead.txt"),
        "committed ahead\n",
    )
    .unwrap();
    commit_workdir(&fixture, "ahead");
    std::fs::write(fixture.repo_dir.path().join("README.md"), "dirty\n").unwrap();

    let against_feature = branch_diff(&engine, &fixture.repo_path(), "feature")
        .await
        .unwrap();
    let against_main = branch_diff(&engine, &fixture.repo_path(), "main")
        .await
        .unwrap();
    assert_ne!(against_feature.checksum, against_main.checksum);
    assert!(against_feature.patch.contains("+committed ahead"));
    assert!(against_main.patch.contains("+dirty"));
    assert!(!against_main.patch.contains("+committed ahead"));
}

#[tokio::test]
async fn branch_scope_rejects_missing_and_unknown_base_refs() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // No baseRef at all: bad params.
    let error = match engine
        .handle(
            methods::GET_CHECKOUT_DIFF,
            serde_json::json!({
                "cwd": fixture.repo_path(),
                "mode": "branch",
                "chatId": "chat-1",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("branch mode without baseRef must fail"),
    };
    assert!(matches!(error, RpcError::BadParams(_)));

    // Unknown base ref: bad params naming it.
    let error = match branch_diff(&engine, &fixture.repo_path(), "no-such-base").await {
        Err(error) => error,
        Ok(_) => panic!("unknown baseRef must fail"),
    };
    assert!(matches!(error, RpcError::BadParams(ref message) if message.contains("no-such-base")));
}

#[tokio::test]
async fn branch_scope_surfaces_unrelated_history_as_an_error() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // An orphan branch with no common ancestry with main.
    let repo = fixture.repo();
    let sig = signature();
    {
        let tree = {
            let mut builder = repo.treebuilder(None).unwrap();
            let blob = repo.blob(b"orphan\n").unwrap();
            builder.insert("orphan.txt", blob, 0o100644).unwrap();
            repo.find_tree(builder.write().unwrap()).unwrap()
        };
        repo.commit(
            Some("refs/heads/orphan"),
            &sig,
            &sig,
            "orphan root",
            &tree,
            &[],
        )
        .unwrap();
    }
    drop(repo);

    let error = match branch_diff(&engine, &fixture.repo_path(), "orphan").await {
        Err(error) => error,
        Ok(_) => panic!("unrelated history must fail, not return an empty diff"),
    };
    assert!(matches!(error, RpcError::Failed(ref message) if message.contains("ancestry")));
}

#[tokio::test]
async fn branch_scope_file_text_reads_the_merge_base_blob() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // `feature` changed nothing on main; committing a README change on main
    // makes the merge-base blob (base = feature's parent = the shared base)
    // hold "hello\n" while the workdir holds the edit.
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "edited on the branch\n",
    )
    .unwrap();
    let diff = branch_diff(&engine, &fixture.repo_path(), "feature")
        .await
        .unwrap();

    let request = GetCheckoutFileDiffTextRequest {
        checkout_id: diff.checkout_id.clone(),
        cwd: fixture.repo_path(),
        path: "README.md".into(),
        mode: "branch".into(),
        base_ref: Some("feature".into()),
        chat_id: Some("chat-1".into()),
        commit_sha: None,
        diff_checksum: diff.checksum.clone(),
    };
    let RpcReply::Value(value) = engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::to_value(&request).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!();
    };
    let text: holt_proto::CheckoutFileDiffText = serde_json::from_value(value).unwrap();
    assert_eq!(text.old_text.as_deref(), Some("hello\n"), "merge-base blob");
    assert_eq!(
        text.new_text.as_deref(),
        Some("edited on the branch\n"),
        "working-tree file"
    );
    assert!(!text.stale);
}
