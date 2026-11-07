//! The integration checks surface (P2): the trunk's `.holt/checks.toml`
//! executed against a worktree's in-memory squash merge in a throwaway
//! checkout. Real git repositories, real shells, driven through the
//! `RpcService` trait.

use crate::common;

use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::checks::CheckRun;
use holt_rpc::{RpcReply, RpcService, methods};
use tempfile::TempDir;

struct Fixture {
    repo_dir: TempDir,
    data_dir: TempDir,
    wt_dir: TempDir,
}

impl Fixture {
    /// main: `root`; a worktree on `feature` one commit ahead.
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = common::init_repo(repo_dir.path());
        repo.set_head("refs/heads/main").unwrap();
        let root = common::commit_on(
            &repo,
            "refs/heads/main",
            None,
            &[("README.md", "hello\n")],
            "root",
        );
        let wt_dir = TempDir::new().unwrap();
        let feature_path = wt_dir.path().join("wt-feature");
        {
            let feature = repo.find_commit(root).unwrap();
            repo.branch("feature", &feature, false).unwrap();
        }
        let reference = repo.find_reference("refs/heads/feature").unwrap();
        let mut options = git2::WorktreeAddOptions::new();
        options.reference(Some(&reference));
        repo.worktree("wt-feature", &feature_path, Some(&options))
            .unwrap();
        drop(reference);
        {
            let worktree_repo = Repository::open(&feature_path).unwrap();
            common::commit_on(
                &worktree_repo,
                "refs/heads/feature",
                Some(root),
                &[("README.md", "hello\n"), ("feature.txt", "feature\n")],
                "feature work",
            );
        }
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        repo.checkout_head(Some(&mut checkout)).unwrap();
        drop(repo);
        Self {
            repo_dir,
            data_dir: TempDir::new().unwrap(),
            wt_dir,
        }
    }

    fn engine(&self) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: None,
            stream_fn: None,
            search_backend_resolver: None,
            clock: None,
            exec_shell: None,
        })
        .unwrap()
    }

    fn repo_path(&self) -> String {
        self.repo_dir.path().display().to_string()
    }

    fn repo(&self) -> Repository {
        Repository::open(self.repo_dir.path()).unwrap()
    }
}

/// Commit `entries` onto `refs/heads/main` (the trunk), leaving the main
/// checkout clean.
fn commit_trunk(fixture: &Fixture, entries: &[(&str, &str)], message: &str) -> git2::Oid {
    let repo = fixture.repo();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let oid = common::commit_on(&repo, "refs/heads/main", Some(tip), entries, message);
    drop(repo);
    oid
}

/// Write `.holt/checks.toml` onto the trunk (and only there).
fn commit_checks_config(fixture: &Fixture, body: &str) -> git2::Oid {
    let repo = fixture.repo();
    let tip_commit = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    // treebuilder entries are names, not paths: build .holt as a subtree
    // and graft it into the root.
    let blob = repo.blob(body.as_bytes()).unwrap();
    let mut holt = repo.treebuilder(None).unwrap();
    holt.insert("checks.toml", blob, 0o100644).unwrap();
    let holt_tree = holt.write().unwrap();
    let mut builder = repo.treebuilder(Some(&tip_commit.tree().unwrap())).unwrap();
    builder.insert(".holt", holt_tree, 0o040000).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = common::signature();
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "declare checks",
        &tree,
        &[&tip_commit],
    )
    .unwrap()
}

async fn run_checks(engine: &LocalEngine, fixture: &Fixture) -> Result<String, holt_rpc::RpcError> {
    match engine
        .handle(
            methods::RUN_CHECKS,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-feature",
                "trunk": "main",
            }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(value["runId"].as_str().unwrap().to_string()),
        Ok(_) => panic!("RunChecks did not return a value"),
        Err(error) => Err(error),
    }
}

/// Subscribe and collect watch frames until `done` returns Some.
async fn watch_until(
    engine: &LocalEngine,
    fixture: &Fixture,
    done: impl Fn(&CheckRun) -> bool,
) -> CheckRun {
    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_CHECKS,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-feature",
            }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchChecks did not return a stream");
    };
    use futures::StreamExt as _;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), stream.next())
            .await
            .expect("a frame within the timeout")
            .expect("the stream stays open");
        if frame.is_null() {
            continue;
        }
        let run: CheckRun = serde_json::from_value(frame).unwrap();
        if done(&run) {
            return run;
        }
    }
}

fn row<'a>(run: &'a CheckRun, name: &str) -> &'a holt_rpc::checks::CheckRow {
    run.checks
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("{name} missing: {:?}", run.checks))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_run_never_clobbers_or_stops_its_replacement() {
    let fixture = Fixture::new();
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"hang\"\nrun = \"sleep 30\"\n[[check]]\nname = \"after\"\nrun = \"true\"\n",
    );
    let engine = fixture.engine();

    let first = run_checks(&engine, &fixture).await.unwrap();
    let _ = watch_until(&engine, &fixture, |run| {
        row(run, "hang").status == "running"
    })
    .await;

    // Re-run: the replacement takes the key and the old id is forgotten.
    let second = run_checks(&engine, &fixture).await.unwrap();
    assert_ne!(first, second);
    let _ = watch_until(&engine, &fixture, |run| run.run_id == second).await;

    // Stopping with the SUPERSEDED id is a no-op: the replacement keeps
    // running.
    engine
        .handle(methods::STOP_CHECKS, serde_json::json!({ "runId": first }))
        .await
        .unwrap();
    let run = watch_until(&engine, &fixture, |run| run.run_id == second).await;
    assert_eq!(
        row(&run, "hang").status,
        "running",
        "a stale id must not stop the replacement: {:?}",
        run.checks
    );

    // A log subscription names the running check or refuses; a stale id
    // never streams the replacement's process.
    let error = match engine
        .handle(
            methods::SUBSCRIBE_CHECK_LOG,
            serde_json::json!({
                "runId": second,
                "name": "after",
                "afterSeq": 0,
            }),
        )
        .await
    {
        Ok(_) => panic!("a not-yet-running check must refuse to stream"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("is not live"),
        "a not-yet-running check must refuse: {error}"
    );

    // The replacement finishes on its own terms and its result survives
    // the superseded run's unwind (no clobber, no removal).
    engine
        .handle(methods::STOP_CHECKS, serde_json::json!({ "runId": second }))
        .await
        .unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert_eq!(
        run.run_id, second,
        "the persisted result is the replacement's"
    );
    assert_eq!(row(&run, "hang").status, "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checks_run_in_a_checkout_of_the_merged_tree() {
    let fixture = Fixture::new();
    // The trunk moves ahead with its own file AND its own checks config;
    // the branch's config edit must not matter (it has none).
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"where\"\nrun = \"pwd && git rev-parse HEAD && ls\"\n",
    );
    commit_trunk(
        &fixture,
        &[("README.md", "hello\n"), ("trunk.txt", "t\n")],
        "trunk moves",
    );
    let engine = fixture.engine();

    run_checks(&engine, &fixture).await.unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;

    assert!(run.error.is_none(), "{:?}", run.error);
    let row = row(&run, "where");
    assert_eq!(row.status, "passed", "{:?}", run.checks);
    // The checkout ran on the SQUASH of trunk+branch: the branch's file,
    // the trunk's file, and the trunk's config are all present, and HEAD
    // is the merge's temp commit — not the branch head, not the trunk.
    assert!(
        row.log_tail.contains("feature.txt"),
        "tail: {}",
        row.log_tail
    );
    assert!(row.log_tail.contains("trunk.txt"), "tail: {}", row.log_tail);
    let branch_head = {
        let repo = fixture.repo();
        repo.find_reference("refs/heads/feature")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string()
    };
    assert!(
        !row.log_tail.contains(&branch_head[..7]),
        "HEAD must not be the branch head: {}",
        row.log_tail
    );
    // The throwaway checkout is gone.
    let checkouts = fixture.data_dir.path().join("integration/checkouts");
    let leftovers: Vec<_> = std::fs::read_dir(&checkouts)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "checkouts cleaned: {leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_and_passing_rows_carry_status_and_exit_codes() {
    let fixture = Fixture::new();
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"ok\"\nrun = \"true\"\n[[check]]\nname = \"bad\"\nrun = \"exit 3\"\n",
    );
    let engine = fixture.engine();
    run_checks(&engine, &fixture).await.unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert_eq!(row(&run, "ok").status, "passed");
    let bad = row(&run, "bad");
    assert_eq!(bad.status, "failed");
    assert_eq!(bad.exit_code, Some(3));
    assert!(run.finished_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_marks_the_row_and_kills_the_group() {
    let fixture = Fixture::new();
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"hang\"\nrun = \"sleep 30\"\ntimeout_secs = 1\n",
    );
    let engine = fixture.engine();
    let run_id = run_checks(&engine, &fixture).await.unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    let row = row(&run, "hang");
    assert_eq!(row.status, "timeout");
    // The process group died: no orphan shell survives (unix).
    #[cfg(unix)]
    {
        let mut settled = Vec::new();
        for _ in 0..50 {
            settled = std::fs::read_dir(fixture.data_dir.path().join("shell-groups"))
                .map(|entries| entries.filter_map(Result::ok).collect::<Vec<_>>())
                .unwrap_or_default();
            if settled.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            settled.is_empty(),
            "the group record was forgotten: {settled:?} (run {run_id})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_marks_stopped_and_cleans_the_checkout() {
    let fixture = Fixture::new();
    commit_checks_config(&fixture, "[[check]]\nname = \"hang\"\nrun = \"sleep 30\"\n");
    let engine = fixture.engine();
    let run_id = run_checks(&engine, &fixture).await.unwrap();
    // Wait until the check is running, then stop.
    let _running = watch_until(&engine, &fixture, |run| {
        run.checks
            .first()
            .is_some_and(|row| row.status == "running")
    })
    .await;
    engine
        .handle(methods::STOP_CHECKS, serde_json::json!({ "runId": run_id }))
        .await
        .unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert_eq!(row(&run, "hang").status, "stopped");
    let checkouts = fixture.data_dir.path().join("integration/checkouts");
    let leftovers: Vec<_> = std::fs::read_dir(&checkouts)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "checkouts cleaned: {leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicting_preview_errors_the_run_without_running_anything() {
    let fixture = Fixture::new();
    // Both sides edit the same line.
    let wt_path = fixture.wt_dir.path().join("wt-feature");
    {
        let worktree_repo = Repository::open(&wt_path).unwrap();
        let feature_tip = worktree_repo
            .find_reference("refs/heads/feature")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        common::commit_on(
            &worktree_repo,
            "refs/heads/feature",
            Some(feature_tip),
            &[
                ("README.md", "branch's line\n"),
                ("feature.txt", "feature\n"),
            ],
            "branch edits the line",
        );
    }
    commit_trunk(
        &fixture,
        &[("README.md", "trunk's line\n")],
        "trunk edits the line",
    );
    commit_checks_config(&fixture, "[[check]]\nname = \"never\"\nrun = \"true\"\n");
    let engine = fixture.engine();

    run_checks(&engine, &fixture).await.unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("conflicts")),
        "{:?}",
        run.error
    );
    // The command never ran: the row stayed pending.
    assert_eq!(row(&run, "never").status, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_head_marks_the_persisted_run_stale() {
    let fixture = Fixture::new();
    commit_checks_config(&fixture, "[[check]]\nname = \"ok\"\nrun = \"true\"\n");
    let engine = fixture.engine();
    run_checks(&engine, &fixture).await.unwrap();
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert!(!run.stale);

    // The trunk moves: the persisted run reads stale on the next watch.
    commit_trunk(
        &fixture,
        &[("README.md", "hello\n"), ("later.txt", "l\n")],
        "later",
    );
    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_CHECKS,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-feature",
            }),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    use futures::StreamExt as _;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap();
    let run: CheckRun = serde_json::from_value(frame).unwrap();
    assert!(run.stale, "a moved trunk marks the persisted run stale");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_checks_toml_means_a_null_watch_and_a_failed_run() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_CHECKS,
            serde_json::json!({
                "repoPath": fixture.repo_path(),
                "worktree": "wt-feature",
            }),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    use futures::StreamExt as _;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(frame.is_null(), "no config, no run: {frame}");

    let error = match run_checks(&engine, &fixture).await {
        Ok(_) => panic!("running without a config must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("checks.toml"), "got: {error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_log_streams_and_the_config_rejects_unknown_fields() {
    let fixture = Fixture::new();
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"say\"\nrun = \"printf checks-ran\"\n",
    );
    let engine = fixture.engine();
    let run_id = run_checks(&engine, &fixture).await.unwrap();
    // Subscribe while the run is live — the log replays then follows.
    let subscribed = engine
        .handle(
            methods::SUBSCRIBE_CHECK_LOG,
            serde_json::json!({ "runId": run_id, "name": "say", "afterSeq": 0 }),
        )
        .await;
    let mut log = match subscribed {
        Ok(RpcReply::Stream(stream)) => Some(stream),
        // The run may finish before we subscribe; the persisted tail is
        // the proof there.
        Ok(_) | Err(_) => None,
    };
    use futures::StreamExt as _;
    let mut saw_output = false;
    if let Some(log) = log.as_mut() {
        while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(15), log.next())
            .await
            .expect("log frames")
        {
            if event
                .get("data")
                .and_then(|data| data.as_str())
                .is_some_and(|data| data.contains("Y2hlY2tzLXJhbg"))
            {
                saw_output = true;
            }
            if event.get("exit").is_some() {
                break;
            }
        }
    }
    let run = watch_until(&engine, &fixture, |run| run.finished()).await;
    assert!(
        saw_output || row(&run, "say").log_tail.contains("checks-ran"),
        "the output reached the log or the tail"
    );

    // Unknown fields are errors, not silently-ignored typos.
    commit_checks_config(
        &fixture,
        "[[check]]\nname = \"x\"\nrun = \"true\"\ncolour = \"red\"\n",
    );
    drop(engine);
    let engine = fixture.engine();
    let error = match run_checks(&engine, &fixture).await {
        Ok(_) => panic!("an unknown field must fail the run"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("colour"), "got: {error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_leftover_checkouts_are_pruned_and_never_listed() {
    let fixture = Fixture::new();
    let engine = fixture.engine();

    // A crashed run's leftover: a registered checks worktree whose
    // directory is gone (clean_checkouts removed the directory at boot; the
    // registration survived).
    let leftover = fixture
        .data_dir
        .path()
        .join("integration/checkouts/dead-run");
    std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
    let repo = fixture.repo();
    let tip_id = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    {
        let tip = repo.find_commit(tip_id).unwrap();
        repo.branch("holt/check/dead-run", &tip, false).unwrap();
    }
    let reference = repo
        .find_reference("refs/heads/holt/check/dead-run")
        .unwrap();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repo.worktree("dead-run", &leftover, Some(&options))
        .unwrap();
    drop(reference);
    drop(repo);
    std::fs::remove_dir_all(&leftover).unwrap();

    let run = list_integration(&engine, &fixture).await;
    assert!(
        run.worktrees.iter().all(|row| row.name != "dead-run"),
        "crash leftovers never list: {:?}",
        run.worktrees
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<_>>()
    );
    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(
        !names.contains(&"dead-run".to_string()),
        "the leftover registration was pruned: {names:?}"
    );
    // The temp branch died with it.
    assert!(
        repo.find_reference("refs/heads/holt/check/dead-run")
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_checks_checkout_is_invisible_but_untouched() {
    let fixture = Fixture::new();
    commit_checks_config(&fixture, "[[check]]\nname = \"hang\"\nrun = \"sleep 30\"\n");
    let engine = fixture.engine();
    let run_id = run_checks(&engine, &fixture).await.unwrap();
    // Wait until the run's checkout exists (the check is running in it).
    let checkouts = fixture.data_dir.path().join("integration/checkouts");
    let mut live = checkouts.join(&run_id);
    for _ in 0..100 {
        live = checkouts.join(&run_id);
        if live.join(".git").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(live.join(".git").exists(), "the checkout exists: {live:?}");

    let run = list_integration(&engine, &fixture).await;
    assert!(
        run.worktrees.iter().all(|row| row.name != run_id),
        "a live checks checkout never lists: {:?}",
        run.worktrees
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<_>>()
    );
    let repo = fixture.repo();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(
        names.contains(&run_id),
        "the LIVE registration is untouched: {names:?}"
    );
    assert!(
        live.join(".git").exists(),
        "the live checkout directory is untouched"
    );

    // Wind down: stop the run and let the cleanup finish.
    engine
        .handle(methods::STOP_CHECKS, serde_json::json!({ "runId": run_id }))
        .await
        .unwrap();
    let _ = watch_until(&engine, &fixture, |run| run.finished()).await;
}

async fn list_integration(
    engine: &LocalEngine,
    fixture: &Fixture,
) -> holt_proto::IntegrationSnapshot {
    let RpcReply::Value(value) = engine
        .handle(
            methods::LIST_INTEGRATION,
            serde_json::json!({ "repoPath": fixture.repo_path() }),
        )
        .await
        .unwrap()
    else {
        panic!("ListIntegration did not return a value");
    };
    serde_json::from_value(value).unwrap()
}
