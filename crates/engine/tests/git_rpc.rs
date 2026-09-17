//! Handle-seam tests for the git capability: a real engine assembled on a
//! temp data dir, a real fixture repository built with git2 in another temp
//! dir, driven through the `RpcService` trait exactly as the UI drives it.
//! The fixture repos have no remotes unless a test adds one — branch listing
//! and switching must work fully offline.

mod common;

use futures::StreamExt as _;
use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
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

    fn engine(&self) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: Some(
                common::ScriptedProvider::new(
                    (0..8)
                        .map(|_| common::ScriptedReply::text("done"))
                        .collect(),
                )
                .stream_fn(),
            ),
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

async fn list_refs(engine: &LocalEngine, repo_path: &str) -> Vec<(String, bool, Option<String>)> {
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

async fn register_space(engine: &LocalEngine, fixture: &Fixture, space_id: &str) {
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

async fn first_space(engine: &LocalEngine) -> Space {
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

async fn working_tree_diff(engine: &LocalEngine, cwd: &str) -> CheckoutDiff {
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
        message_id: None,
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

async fn branch_diff(
    engine: &LocalEngine,
    cwd: &str,
    base: &str,
) -> Result<CheckoutDiff, RpcError> {
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
        message_id: None,
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

// ---- history, fetch, per-commit diffs (git-capability issue 05) ----

fn ref_kind(reference: &holt_proto::GitHistoryRef) -> String {
    serde_json::to_value(reference.kind)
        .unwrap()
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn history_page(
    engine: &LocalEngine,
    cwd: &str,
    cursor: u64,
    limit: u64,
) -> holt_proto::GitHistoryPage {
    let RpcReply::Value(value) = engine
        .handle(
            methods::LIST_GIT_HISTORY,
            serde_json::json!({ "cwd": cwd, "cursor": cursor, "limit": limit }),
        )
        .await
        .unwrap()
    else {
        panic!("ListGitHistory did not return a value");
    };
    serde_json::from_value(value).unwrap()
}

async fn commit_diff(engine: &LocalEngine, cwd: &str, sha: &str) -> Result<CheckoutDiff, RpcError> {
    match engine
        .handle(
            methods::GET_CHECKOUT_DIFF,
            serde_json::json!({
                "cwd": cwd,
                "mode": "commit",
                "commitSha": sha,
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

#[tokio::test]
async fn history_pages_topologically_with_refs_and_counts() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // A merge topology: a feature branch commit merged back into main.
    let (main_tip, merged_oid) = {
        let repo = fixture.repo();
        let sig = signature();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        let feature_tip = repo
            .find_reference("refs/heads/feature")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let feature_oid = feature_tip.id();
        let merged_oid = {
            let mut index = repo.index().unwrap();
            index.read_tree(&feature_tip.tree().unwrap()).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(
                Some("refs/heads/merge-src"),
                &sig,
                &sig,
                "work on the branch",
                &tree,
                &[&head_commit],
            )
            .unwrap()
        };
        // main tip becomes the merge commit
        let main_tip = {
            let merged_commit = repo.find_commit(merged_oid).unwrap();
            let tree = merged_commit.tree().unwrap();
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "merge the branch",
                &tree,
                &[&head_commit, &merged_commit],
            )
            .unwrap()
        };
        // Refs: a tag on the merge and a remote-tracking ref on the side
        // branch's own commit.
        repo.reference("refs/tags/v1", main_tip, true, "test")
            .unwrap();
        repo.reference("refs/remotes/origin/merge-src", merged_oid, true, "test")
            .unwrap();
        let _ = feature_oid;
        (main_tip, merged_oid)
    };

    let page = history_page(&engine, &fixture.repo_path(), 0, 2).await;
    assert_eq!(page.commits.len(), 2);
    assert_eq!(page.next_cursor, Some(2));
    assert!(page.total_count.is_some());
    assert_eq!(page.head_commit_count, page.total_count);
    let head = &page.commits[0];
    assert_eq!(head.sha, main_tip.to_string());
    assert_eq!(head.subject, "merge the branch");
    assert_eq!(head.author_name, "Holt Test");
    assert!(head.authored_at.contains('T'), "RFC3339 authoredAt");
    assert_eq!(head.parent_shas.len(), 2, "the merge carries both parents");
    // Both a branch ref (main) and the tag name the merge commit.
    let labels: Vec<(String, String)> = head
        .refs
        .iter()
        .map(|r| (ref_kind(r), r.label.clone()))
        .collect();
    assert!(
        labels.contains(&("branch".to_string(), "main".to_string())),
        "labels: {labels:?}"
    );
    assert!(
        labels.contains(&("tag".to_string(), "v1".to_string())),
        "labels: {labels:?}"
    );
    // The side branch's commit carries branch + remote labels (it sits on
    // page one or two depending on the topo order's parent preference).
    let page2 = history_page(&engine, &fixture.repo_path(), 2, 50).await;
    let merged_row = page
        .commits
        .iter()
        .chain(page2.commits.iter())
        .find(|commit| commit.sha == merged_oid.to_string())
        .expect("side-branch commit present");
    let labels: Vec<(String, String)> = merged_row
        .refs
        .iter()
        .map(|r| (ref_kind(r), r.label.clone()))
        .collect();
    assert!(
        labels.contains(&("branch".to_string(), "merge-src".to_string())),
        "labels: {labels:?}"
    );
    assert!(
        labels.contains(&("remote".to_string(), "origin/merge-src".to_string())),
        "labels: {labels:?}"
    );
}

#[tokio::test]
async fn history_paging_by_cursor_returns_disjoint_pages() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    // Two commits on main so a page split has something to split.
    std::fs::write(fixture.repo_dir.path().join("second.txt"), "second\n").unwrap();
    commit_workdir(&fixture, "second commit");

    let first = history_page(&engine, &fixture.repo_path(), 0, 1).await;
    assert_eq!(first.commits.len(), 1);
    assert_eq!(first.next_cursor, Some(1));

    let second = history_page(
        &engine,
        &fixture.repo_path(),
        first.next_cursor.unwrap() as u64,
        50,
    )
    .await;
    assert_eq!(second.next_cursor, None);
    let first_shas: Vec<&str> = first.commits.iter().map(|c| c.sha.as_str()).collect();
    assert!(
        second
            .commits
            .iter()
            .all(|c| !first_shas.contains(&c.sha.as_str())),
        "pages are disjoint"
    );
    // The graph parent links resolve inside the union of the pages.
    let all: Vec<&holt_proto::GitHistoryCommit> =
        first.commits.iter().chain(second.commits.iter()).collect();
    for commit in &all {
        for parent in &commit.parent_shas {
            assert!(
                all.iter().any(|c| &c.sha == parent),
                "parent {parent} of {} missing",
                commit.sha
            );
        }
    }
}

#[tokio::test]
async fn commit_mode_diffs_parent_to_commit_without_the_worktree() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    let repo = fixture.repo();
    let (feature_oid, root_oid) = {
        let feature_tip = repo
            .find_reference("refs/heads/feature")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let root = repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        (feature_tip.id(), root.id())
    };
    drop(repo);

    // Dirt that must NOT leak into the pinned commit pair.
    std::fs::write(fixture.repo_dir.path().join("README.md"), "live edits\n").unwrap();

    let diff = commit_diff(&engine, &fixture.repo_path(), &feature_oid.to_string())
        .await
        .unwrap();
    assert!(diff.patch.contains("+feature work"));
    assert!(
        !diff.patch.contains("+live edits"),
        "the workdir is never read"
    );
    // The root commit diffs against the empty tree: everything is an add.
    let root_diff = commit_diff(&engine, &fixture.repo_path(), &root_oid.to_string())
        .await
        .unwrap();
    assert!(root_diff.files.iter().all(|file| file.status == "added"));

    // Unknown commit: bad params naming it.
    let error = match commit_diff(
        &engine,
        &fixture.repo_path(),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("unknown commit must fail"),
    };
    assert!(matches!(error, RpcError::BadParams(_)));
}

#[tokio::test]
async fn commit_mode_file_text_reads_parent_and_commit_blobs() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    let repo = fixture.repo();
    let feature_oid = repo
        .find_reference("refs/heads/feature")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    drop(repo);
    let diff = commit_diff(&engine, &fixture.repo_path(), &feature_oid.to_string())
        .await
        .unwrap();

    let request = GetCheckoutFileDiffTextRequest {
        checkout_id: diff.checkout_id.clone(),
        cwd: fixture.repo_path(),
        path: "feature.txt".into(),
        mode: "commit".into(),
        base_ref: None,
        chat_id: Some("chat-1".into()),
        commit_sha: Some(feature_oid.to_string()),
        message_id: None,
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
    assert_eq!(text.old_text, None, "the parent does not know the file");
    assert_eq!(text.new_text.as_deref(), Some("feature work\n"));
    assert!(!text.stale);
}

#[tokio::test]
async fn fetch_all_updates_remote_refs_and_prunes_without_touching_the_checkout() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // A local bare remote (file transport, no network) with a branch of
    // its own, plus a stale remote-tracking ref to prune.
    let remote_dir = TempDir::new().unwrap();
    let bare = Repository::init_bare(remote_dir.path()).unwrap();
    drop(bare);
    {
        let repo = fixture.repo();
        let stale_ref = repo
            .find_reference("refs/heads/feature")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference("refs/remotes/origin/feature", stale_ref, true, "stale")
            .unwrap();
        repo.remote("origin", &format!("file://{}", remote_dir.path().display()))
            .unwrap();
        drop(repo);
    }
    // Push main into the bare remote so the fetch has something to bring.
    {
        let repo = fixture.repo();
        let mut origin = repo.find_remote("origin").unwrap();
        origin
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();
    }

    // The working-tree checksum before the fetch must survive it.
    let before = working_tree_diff(&engine, &fixture.repo_path()).await;
    engine
        .handle(
            methods::FETCH_ALL,
            serde_json::json!({ "repoPath": fixture.repo_path() }),
        )
        .await
        .unwrap();
    let after = working_tree_diff(&engine, &fixture.repo_path()).await;
    assert_eq!(
        before.checksum, after.checksum,
        "fetch mutates no checkout state"
    );
    let repo = fixture.repo();
    assert!(
        repo.find_reference("refs/remotes/origin/main").is_ok(),
        "the remote branch landed as a remote-tracking ref"
    );
    assert!(
        repo.find_reference("refs/remotes/origin/feature").is_err(),
        "the stale tracking ref was pruned"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
}

#[tokio::test]
async fn fetch_all_surfaces_errors_verbatim() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // A remote pointing nowhere: the fetch fails and the message carries
    // the remote's name.
    let repo = fixture.repo();
    repo.remote("broken", "file:///nonexistent/remote/path")
        .unwrap();
    drop(repo);
    let error = match engine
        .handle(
            methods::FETCH_ALL,
            serde_json::json!({ "repoPath": fixture.repo_path() }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("a broken remote must fail"),
    };
    assert!(matches!(error, RpcError::Failed(_)));
    assert!(
        error.to_string().contains("broken"),
        "the remote name rides along: {error}"
    );
}

// ---- latest turn: net-change diffs (git-capability issue 06) ----

/// A real admitted Turn establishes the baseline; pending or invalid
/// requests cannot change the previous Turn's diff.
async fn complete_turn(engine: &LocalEngine, chat_id: &str, cwd: &str) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({"providerId":"openai","key":"test-only"}),
        )
        .await
        .unwrap();
    let result = engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "request": {
                        "prompt": "do the thing",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": cwd,
                        "sandbox": "workspace-write",
                    }
                }
            }),
        )
        .await;
    result.unwrap();
    let RpcReply::Stream(mut queue) = engine
        .handle(
            methods::WATCH_MESSAGE_QUEUE,
            serde_json::json!({"chatId":chat_id}),
        )
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let state = common::next_frame(&mut queue).await;
        assert_ne!(state["paused"], true, "Turn failed: {state}");
        if state["pending"] == serde_json::json!([]) && state["activeMessageId"].is_null() {
            break;
        }
    }
}

async fn turn_diff(
    engine: &LocalEngine,
    cwd: &str,
    chat_id: &str,
) -> Result<CheckoutDiff, RpcError> {
    match engine
        .handle(
            methods::GET_CHECKOUT_DIFF,
            serde_json::json!({
                "cwd": cwd,
                "mode": "turn",
                "chatId": chat_id,
            }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("GetCheckoutDiff did not return a value"),
        Err(error) => Err(error),
    }
}

#[tokio::test]
async fn turn_without_a_baseline_is_an_explicit_error_not_an_empty_diff() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    let error = match turn_diff(&engine, &fixture.repo_path(), "chat-1").await {
        Err(error) => error,
        Ok(_) => panic!("no turn recorded must be an error"),
    };
    assert!(
        error.to_string().contains("no turn recorded"),
        "the UI soft-matches this phrase: {error}"
    );
}

#[tokio::test]
async fn an_admitted_turn_records_a_baseline() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // The scripted Turn starts and establishes the baseline.
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;
    let diff = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .expect("baseline recorded at admission");
    assert!(diff.patch.trim().is_empty(), "clean tree at turn start");
}

#[tokio::test]
async fn a_pending_message_refreshes_branch_and_diff_only_when_its_turn_starts() {
    use std::sync::Arc;
    let fixture = Fixture::new();
    let first = Arc::new(tokio::sync::Notify::new());
    let second = Arc::new(tokio::sync::Notify::new());
    let provider = common::ScriptedProvider::new(vec![
        common::ScriptedReply::gated(first.clone(), "A done"),
        common::ScriptedReply::gated(second.clone(), "B done"),
    ]);
    let engine = LocalEngine::assemble(&EngineConfig {
        data_dir: fixture.data_dir.path().into(),
        personal_skills_dir: None,
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
    })
    .unwrap();
    register_space(&engine, &fixture, "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({"providerId":"openai","key":"test-only"}),
        )
        .await
        .unwrap();
    common::run_prompt(&engine, "chat-1", &fixture.repo_path(), "A").await;
    common::wait_for_requests(&provider, 1).await;
    common::run_prompt(&engine, "chat-1", &fixture.repo_path(), "B").await;
    engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({"repoPath":fixture.repo_path(),"refName":"feature"}),
        )
        .await
        .unwrap();
    assert_eq!(
        chat_row(&engine, "chat-1").await.branch.as_deref(),
        Some("main")
    );
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "change before B\n",
    )
    .unwrap();
    assert!(
        turn_diff(&engine, &fixture.repo_path(), "chat-1")
            .await
            .unwrap()
            .patch
            .contains("change before B")
    );
    first.notify_one();
    common::wait_for_requests(&provider, 2).await;
    assert_eq!(
        chat_row(&engine, "chat-1").await.branch.as_deref(),
        Some("feature")
    );
    assert!(
        turn_diff(&engine, &fixture.repo_path(), "chat-1")
            .await
            .unwrap()
            .patch
            .is_empty()
    );
    second.notify_one();
}

#[tokio::test]
async fn turn_diff_on_a_clean_start_shows_changes_since_the_turn_began() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;

    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "agent edits live\n",
    )
    .unwrap();
    let diff = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .unwrap();
    assert!(diff.patch.contains("-hello"), "{}", diff.patch);
    assert!(diff.patch.contains("+agent edits live"), "{}", diff.patch);
    assert_eq!(diff.files.len(), 1);

    // Live updates: a later edit changes the same query's answer.
    std::fs::write(fixture.repo_dir.path().join("README.md"), "more edits\n").unwrap();
    let again = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .unwrap();
    assert!(again.patch.contains("+more edits"));
    assert_ne!(again.checksum, diff.checksum);
}

#[tokio::test]
async fn turn_diff_filters_net_changes_on_a_dirty_start() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // Pre-turn dirt: one file the turn never touches, one it edits, one it
    // edits and reverts to the exact turn-start bytes.
    std::fs::write(
        fixture.repo_dir.path().join("untouched.txt"),
        "user was here\n",
    )
    .unwrap();
    std::fs::write(fixture.repo_dir.path().join("edited.txt"), "user base\n").unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("reverted.txt"),
        "revert base\n",
    )
    .unwrap();
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;

    // Agent-like edits: touch `edited`, wiggle `reverted` back to its
    // turn-start bytes, create a new file, rename a tracked one.
    std::fs::write(
        fixture.repo_dir.path().join("edited.txt"),
        "user base\nagent touched\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("reverted.txt"),
        "revert base\nagent\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("reverted.txt"),
        "revert base\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("created.txt"),
        "agent made this\n",
    )
    .unwrap();
    std::fs::rename(
        fixture.repo_dir.path().join("movable.txt"),
        fixture.repo_dir.path().join("moved-away.txt"),
    )
    .unwrap();

    let diff = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .unwrap();
    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert!(
        !paths.contains(&"untouched.txt"),
        "pre-existing dirt the turn never touched stays out: {paths:?}"
    );
    assert!(
        !paths.contains(&"reverted.txt"),
        "net-zero revert drops: {paths:?}"
    );
    assert!(
        paths.contains(&"edited.txt"),
        "touched dirty file stays: {paths:?}"
    );
    assert!(paths.contains(&"created.txt"), "new file stays: {paths:?}");
    assert!(paths.contains(&"moved-away.txt"), "rename stays: {paths:?}");
    assert!(diff.patch.contains("+agent made this"));
    assert!(diff.patch.contains("rename from movable.txt"));
}

#[tokio::test]
async fn turn_baseline_dies_with_the_engine() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;
    drop(engine);

    // A fresh engine on the same data dir: in-memory baselines are gone.
    let engine = fixture.engine();
    let error = match turn_diff(&engine, &fixture.repo_path(), "chat-1").await {
        Err(error) => error,
        Ok(_) => panic!("restart must drop baselines"),
    };
    assert!(error.to_string().contains("no turn recorded"));
}

#[tokio::test]
async fn turn_file_text_reads_turn_start_content_and_the_workdir() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // The file is dirty at turn start; the agent edits it further.
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "dirty at start\n",
    )
    .unwrap();
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;
    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "dirty at start\nagent added\n",
    )
    .unwrap();

    let diff = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .unwrap();
    let request = GetCheckoutFileDiffTextRequest {
        checkout_id: diff.checkout_id.clone(),
        cwd: fixture.repo_path(),
        path: "README.md".into(),
        mode: "turn".into(),
        base_ref: None,
        chat_id: Some("chat-1".into()),
        commit_sha: None,
        message_id: None,
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
    assert_eq!(
        text.old_text.as_deref(),
        Some("dirty at start\n"),
        "old side is the turn-start content, not the HEAD blob"
    );
    assert_eq!(
        text.new_text.as_deref(),
        Some("dirty at start\nagent added\n"),
        "new side is the working-tree file"
    );
}

#[tokio::test]
async fn turn_baseline_patch_rides_the_three_mib_cap() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;

    // Dirt far past the cap at queue time: the baseline records truncated,
    // and the turn scope still answers.
    let huge = "x".repeat(6 * 1024 * 1024);
    std::fs::write(fixture.repo_dir.path().join("huge.txt"), &huge).unwrap();
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;

    std::fs::write(
        fixture.repo_dir.path().join("README.md"),
        "post-queue edit\n",
    )
    .unwrap();
    let diff = turn_diff(&engine, &fixture.repo_path(), "chat-1")
        .await
        .expect("the capped baseline still serves the turn scope");
    // The still-changed huge file dominates the capped patch, but the
    // summaries stay complete and the truncation is flagged.
    assert!(diff.truncated);
    assert!(diff.files.iter().any(|file| file.path == "huge.txt"));
    assert!(diff.files.iter().any(|file| file.path == "README.md"));
    assert!(diff.patch.len() <= 4 * 1024 * 1024);
}

// ---- live branch switching: Turn identity stamps (ADR-0007) ----

use holt_proto::Chat;

/// Register a space at an explicit path (the plain `register_space` pins the
/// fixture's repo folder).
async fn register_space_at(engine: &LocalEngine, space_id: &str, path: &str) {
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

async fn create_chat(engine: &LocalEngine, chat_id: &str, space_id: &str) {
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": chat_id,
                "spaceId": space_id,
            }),
        )
        .await
        .unwrap();
}

async fn chat_row(engine: &LocalEngine, chat_id: &str) -> Chat {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    let value = chats.next().await.expect("chats snapshot");
    let chats: Vec<Chat> = serde_json::from_value(value).unwrap();
    chats
        .into_iter()
        .find(|chat| chat.id == chat_id)
        .expect("chat row present")
}

/// Canonicalize for comparison: a temp dir path and the workdir libgit2
/// reports can disagree on symlink prefixes (/var vs /private/var).
fn canon(path: &str) -> String {
    std::fs::canonicalize(path).unwrap().display().to_string()
}

#[tokio::test]
async fn accepted_run_stamps_branch_and_source_context_from_head() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;

    // An admitted Turn lands the identity stamps: they happen
    // synchronously at command acceptance, before validation.
    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;

    let chat = chat_row(&engine, "chat-1").await;
    assert_eq!(chat.branch.as_deref(), Some("main"));
    let source = chat.source_context.expect("source context stamped");
    assert_eq!(source.branch, "main");
    assert_eq!(source.cwd, fixture.repo_path());
    assert_eq!(canon(&source.repo_root), canon(&fixture.repo_path()));
    assert!(
        source.head_sha.is_some(),
        "the stamped HEAD sha rides along"
    );
    // The checkout id matches the identity the space minted for the folder.
    let space = first_space(&engine).await;
    assert_eq!(
        source.checkout_id,
        space.checkout_id.expect("space identity minted")
    );
}

#[tokio::test]
async fn a_run_restamps_the_chat_row_cwd_from_the_request() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    // The chat is minted with a cwd away from the repo folder; the Run's
    // request carries the repo path.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "chat-1",
                "spaceId": "space-1",
                "cwd": "/elsewhere",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        chat_row(&engine, "chat-1").await.cwd.as_deref(),
        Some("/elsewhere"),
        "precondition: creation stamps the passed cwd"
    );

    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;

    let chat = chat_row(&engine, "chat-1").await;
    assert_eq!(
        chat.cwd.as_deref(),
        Some(fixture.repo_path().as_str()),
        "the per-Run cwd restamp follows the request"
    );
}

#[tokio::test]
async fn a_switch_between_runs_restamps_branch_and_source_context() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    register_space(&engine, &fixture, "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;

    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;
    let first = chat_row(&engine, "chat-1").await;
    let first = first.source_context.expect("first stamp landed");

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

    complete_turn(&engine, "chat-1", &fixture.repo_path()).await;
    let chat = chat_row(&engine, "chat-1").await;
    assert_eq!(chat.branch.as_deref(), Some("feature"));
    let second = chat.source_context.expect("second stamp landed");
    assert_eq!(second.branch, "feature");
    assert_eq!(second.cwd, first.cwd, "the working directory never moves");
    assert_eq!(second.repo_root, first.repo_root);
    assert_eq!(second.checkout_id, first.checkout_id);
    assert_ne!(
        second.head_sha, first.head_sha,
        "main and feature point at different commits"
    );
}

#[tokio::test]
async fn a_run_on_a_non_git_folder_leaves_the_identity_unstamped() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let plain = TempDir::new().unwrap();
    let plain_path = plain.path().display().to_string();
    register_space_at(&engine, "space-plain", &plain_path).await;
    create_chat(&engine, "chat-1", "space-plain").await;

    // A non-git folder still admits ordinary Turns.
    complete_turn(&engine, "chat-1", &plain_path).await;

    let chat = chat_row(&engine, "chat-1").await;
    assert_eq!(chat.branch, None, "no branch to stamp on a plain folder");
    assert!(chat.source_context.is_none());
    // The cwd restamp is unconditional: it still follows the request.
    assert_eq!(chat.cwd.as_deref(), Some(plain_path.as_str()));
}

#[tokio::test]
async fn a_dirty_switch_refusal_names_the_blocking_files() {
    let fixture = Fixture::new();
    // A branch that changes two files relative to main: the tracked
    // README (edited) and a fresh delta.txt.
    let repo = fixture.repo();
    let base = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    commit_on(
        &repo,
        "refs/heads/delta",
        Some(base),
        &[("README.md", "delta readme\n"), ("delta.txt", "delta\n")],
        "delta",
    );
    drop(repo);

    let engine = fixture.engine();
    // Local dirt: README.md edited (tracked, the branch changes it too),
    // delta.txt present untracked (the branch would overwrite it), and a
    // scratch file the branch does NOT touch (must not block).
    std::fs::write(fixture.repo_dir.path().join("README.md"), "local edit\n").unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("delta.txt"),
        "precious uncommitted work\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo_dir.path().join("scratch.txt"),
        "dirty but untouched by the branch\n",
    )
    .unwrap();

    let error = match engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "delta",
            }),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("dirty switch must fail"),
    };
    let message = error.to_string();
    assert!(
        message.starts_with("switch refused:"),
        "the refusal marker the UI parses must lead: {message}"
    );
    assert!(
        message.contains("uncommitted changes would be overwritten"),
        "the human-readable explanation rides along: {message}"
    );
    let files: Vec<&str> = message
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        files,
        vec!["README.md", "delta.txt"],
        "exactly the conflicting paths, sorted, one per line: {message}"
    );

    // The refusal left the tree and HEAD where they were.
    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
    assert_eq!(
        std::fs::read_to_string(fixture.repo_dir.path().join("delta.txt")).unwrap(),
        "precious uncommitted work\n"
    );
}

#[tokio::test]
async fn a_clean_switch_between_branches_moves_head() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "alpha",
            }),
        )
        .await
        .unwrap();
    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "alpha");
    // And back — the working tree stays clean through both hops.
    engine
        .handle(
            methods::SWITCH_REF,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "refName": "main",
            }),
        )
        .await
        .unwrap();
    assert_eq!(fixture.repo().head().unwrap().shorthand().unwrap(), "main");
}

#[tokio::test]
async fn a_worktree_hosted_ref_is_refused_by_git_itself() {
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
    repo.worktree("wt-switch", &wt_path, Some(&options))
        .unwrap();
    drop(feature_ref);
    drop(repo);

    let engine = fixture.engine();
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
        Ok(_) => panic!("a ref held by another worktree must not be switchable"),
    };
    // Not a dirty-tree refusal: git's own "checked out elsewhere" rule.
    let message = error.to_string();
    assert!(
        !message.starts_with("switch refused:"),
        "worktree-hosted refs are a different refusal: {message}"
    );
    assert!(
        message.to_lowercase().contains("linked"),
        "git's worktree refusal message expected: {message}"
    );
    // Both checkouts keep their HEAD.
    let repo = fixture.repo();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
    let wt_repo = Repository::open(&wt_path).unwrap();
    assert_eq!(wt_repo.head().unwrap().shorthand().unwrap(), "feature");
}
