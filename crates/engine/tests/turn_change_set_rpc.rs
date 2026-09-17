//! Handle-seam tests for Turn change sets (ADR-0024, tickets 01+02): a real
//! engine on a temp data dir, a real fixture repository built with git2,
//! driven through `RpcService` exactly as the UI drives it. The change set
//! is the net Git change between a Turn's admission baseline and its live
//! or final working tree — separate from the checkout-diff scopes. Ticket 02
//! freezes settled Turns durably: the summary and the immutable per-file
//! before/after content persist under the data dir, survive a restart, and
//! never move with later workspace edits.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedProvider, ScriptedReply, next_frame, run_prompt, wait_for_requests};
use futures::StreamExt as _;
use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_proto::{
    CheckoutFileDiffText, TurnChangeSet, TurnChangeSetPhase, TurnChangeSetReply,
    TurnFileChangeStatus,
};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;
use tokio::sync::Notify;

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
    /// The personal skill root override — pinned empty so the system prompt
    /// stays fixture-driven, not machine-driven.
    personal_dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
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
            personal_dir: TempDir::new().unwrap(),
        }
    }

    fn engine(&self, provider: &ScriptedProvider) -> LocalEngine {
        LocalEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: Some(self.personal_dir.path().to_path_buf()),
            stream_fn: Some(provider.stream_fn()),
            search_backend_resolver: None,
        })
        .unwrap()
    }

    fn repo_path(&self) -> String {
        self.repo_dir.path().display().to_string()
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.repo_dir.path().join(relative)
    }

    /// Commit extra files into main's tree and materialize them.
    fn commit_files(&self, entries: &[(&str, &str)]) {
        let repo = Repository::open(self.repo_dir.path()).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        commit_on(&repo, "refs/heads/main", Some(head.id()), entries, "extra");
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        repo.checkout_head(Some(&mut checkout)).unwrap();
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

async fn register_space(engine: &LocalEngine, path: &str, space_id: &str) {
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

async fn save_key(engine: &LocalEngine) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "test-only" }),
        )
        .await
        .unwrap();
}

async fn get_change_set(
    engine: &LocalEngine,
    chat_id: &str,
) -> Result<TurnChangeSetReply, RpcError> {
    match engine
        .handle(
            methods::GET_TURN_CHANGE_SET,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("GetTurnChangeSet did not return a value"),
        Err(error) => Err(error),
    }
}

/// The `captured` half, or a panic naming the unexpected reply.
async fn captured(engine: &LocalEngine, chat_id: &str) -> TurnChangeSet {
    match get_change_set(engine, chat_id).await.unwrap() {
        TurnChangeSetReply::Captured(change_set) => change_set,
        TurnChangeSetReply::Unsupported { reason } => {
            panic!("expected a change set, got unsupported: {reason}")
        }
    }
}

/// Wait for the queue to settle, asserting it did not pause on an error.
async fn settle_queue(engine: &LocalEngine, chat_id: &str, expect_clean: bool) {
    let RpcReply::Stream(mut queue) = engine
        .handle(
            methods::WATCH_MESSAGE_QUEUE,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let state = next_frame(&mut queue).await;
        if expect_clean {
            assert_ne!(state["paused"], true, "Turn failed: {state}");
        }
        if state["pending"] == serde_json::json!([]) && state["activeMessageId"].is_null() {
            break;
        }
    }
}

fn done_provider() -> ScriptedProvider {
    ScriptedProvider::new((0..8).map(|_| ScriptedReply::text("done")).collect())
}

/// A provider whose single reply is held back until the gate opens — the
/// deterministic "mid-Turn" primitive.
fn gated_provider() -> (ScriptedProvider, Arc<Notify>) {
    let gate = Arc::new(Notify::new());
    (
        ScriptedProvider::new(vec![ScriptedReply::gated(gate.clone(), "done")]),
        gate,
    )
}

/// Register a git space + chat and configure the provider key.
async fn setup(fixture: &Fixture, engine: &LocalEngine) {
    register_space(engine, &fixture.repo_path(), "space-1").await;
    create_chat(engine, "chat-1", "space-1").await;
    save_key(engine).await;
}

/// Queue a `run` command with an explicit message id — the identity the
/// persisted change-set record and the terminal event are keyed by.
async fn queue_run(engine: &LocalEngine, chat_id: &str, cwd: &str, message_id: &str, prompt: &str) {
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id,
                "command": {
                    "kind": "run",
                    "messageId": message_id,
                    "request": {
                        "prompt": prompt,
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "reasoning": null,
                        "modelOptions": {},
                        "cwd": cwd,
                        "sandbox": "workspace-write"
                    }
                }
            }),
        )
        .await
        .unwrap();
}

/// The change set of one specific Turn by message id.
async fn captured_message(
    engine: &LocalEngine,
    chat_id: &str,
    message_id: &str,
) -> Result<TurnChangeSetReply, RpcError> {
    match engine
        .handle(
            methods::GET_TURN_CHANGE_SET,
            serde_json::json!({ "chatId": chat_id, "messageId": message_id }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("GetTurnChangeSet did not return a value"),
        Err(error) => Err(error),
    }
}

/// The turn-scope per-file diff text, addressed by Turn identity.
async fn file_diff_text(
    engine: &LocalEngine,
    chat_id: &str,
    message_id: &str,
    path: &str,
    cwd: &str,
) -> Result<CheckoutFileDiffText, RpcError> {
    match engine
        .handle(
            methods::GET_CHECKOUT_FILE_DIFF_TEXT,
            serde_json::json!({
                "checkoutId": "",
                "cwd": cwd,
                "path": path,
                "mode": "turn",
                "chatId": chat_id,
                "messageId": message_id,
                "diffChecksum": "",
            }),
        )
        .await
    {
        Ok(RpcReply::Value(value)) => Ok(serde_json::from_value(value).unwrap()),
        Ok(_) => panic!("GetCheckoutFileDiffText did not return a value"),
        Err(error) => Err(error),
    }
}

async fn subscribe_events(
    engine: &LocalEngine,
) -> futures::stream::BoxStream<'static, serde_json::Value> {
    let RpcReply::Stream(events) = engine
        .handle(methods::WATCH_TURN_TERMINAL_EVENTS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchTurnTerminalEvents did not return a stream");
    };
    events
}

// ---------------------------------------------------------------------------
// Unsupported and missing-Turn contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_non_git_workspace_reports_unsupported_not_an_empty_change_set() {
    let fixture = Fixture::new();
    let plain = TempDir::new().unwrap();
    let engine = fixture.engine(&done_provider());
    register_space(&engine, &plain.path().display().to_string(), "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;

    match get_change_set(&engine, "chat-1").await.unwrap() {
        TurnChangeSetReply::Unsupported { reason } => {
            assert!(!reason.trim().is_empty(), "unsupported carries a reason");
        }
        TurnChangeSetReply::Captured(change_set) => {
            panic!("a non-Git workspace must never look like an empty change set: {change_set:?}")
        }
    }
}

#[tokio::test]
async fn a_chat_without_a_recorded_turn_is_an_explicit_error() {
    let fixture = Fixture::new();
    let engine = fixture.engine(&done_provider());
    register_space(&engine, &fixture.repo_path(), "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;

    let error = get_change_set(&engine, "chat-1")
        .await
        .expect_err("no Turn recorded must be an error");
    assert!(
        error.to_string().contains("no turn recorded"),
        "the UI soft-matches this phrase: {error}"
    );
}

// ---------------------------------------------------------------------------
// Baseline timing and live/final delivery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_running_turn_reports_a_live_change_set_and_settles_to_final() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "edit the file").await;
    wait_for_requests(&provider, 1).await;

    // A live Turn with no work yet: an empty, still-moving change set.
    let live = captured(&engine, "chat-1").await;
    assert_eq!(live.phase, TurnChangeSetPhase::Live);
    assert!(live.files.is_empty(), "nothing changed yet: {live:?}");

    // The agent edits a tracked file mid-Turn.
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    let live = captured(&engine, "chat-1").await;
    assert_eq!(live.phase, TurnChangeSetPhase::Live);
    assert_eq!(live.files.len(), 1, "{live:?}");
    assert_eq!(live.files[0].path, "README.md");
    assert_eq!(live.files[0].status, TurnFileChangeStatus::Modified);
    assert_eq!(live.additions, 1);
    assert_eq!(live.deletions, 0);

    gate.notify_one();
    settle_queue(&engine, "chat-1", true).await;

    let final_set = captured(&engine, "chat-1").await;
    assert_eq!(final_set.phase, TurnChangeSetPhase::Final);
    assert_eq!(final_set.files, live.files, "the final set is frozen");

    // Later edits do not move the frozen result.
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\nlater\n").unwrap();
    let again = captured(&engine, "chat-1").await;
    assert_eq!(again.files, final_set.files, "final is immutable");
}

// ---------------------------------------------------------------------------
// Dirty start, net changes, renames, binaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dirty_start_excludes_untouched_and_net_zero_files() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    // Pre-Turn dirt: a file the Turn never touches, one it edits, one it
    // edits and reverts to the exact turn-start bytes.
    std::fs::write(fixture.path("untouched.txt"), "user was here\n").unwrap();
    std::fs::write(fixture.path("edited.txt"), "user base\n").unwrap();
    std::fs::write(fixture.path("reverted.txt"), "revert base\n").unwrap();

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "touch files").await;
    wait_for_requests(&provider, 1).await;

    std::fs::write(fixture.path("edited.txt"), "user base\nagent touched\n").unwrap();
    std::fs::write(fixture.path("reverted.txt"), "revert base\nagent\n").unwrap();
    std::fs::write(fixture.path("reverted.txt"), "revert base\n").unwrap();

    gate.notify_one();
    settle_queue(&engine, "chat-1", true).await;

    let change_set = captured(&engine, "chat-1").await;
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
    let paths: Vec<&str> = change_set
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["edited.txt"], "{change_set:?}");
}

#[tokio::test]
async fn renames_are_paired_and_unpairable_moves_fall_back_to_delete_plus_add() {
    let fixture = Fixture::new();
    // A changed move: the content is rewritten, so Git cannot pair it and
    // the fallback reports a separate delete and add.
    fixture.commit_files(&[("tiny.txt", "alpha beta gamma delta epsilon\n")]);
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "move files").await;
    wait_for_requests(&provider, 1).await;

    std::fs::rename(fixture.path("movable.txt"), fixture.path("moved-away.txt")).unwrap();
    std::fs::remove_file(fixture.path("tiny.txt")).unwrap();
    std::fs::write(
        fixture.path("tiny-elsewhere.txt"),
        "nothing like the original remains here\n",
    )
    .unwrap();

    gate.notify_one();
    settle_queue(&engine, "chat-1", true).await;

    let change_set = captured(&engine, "chat-1").await;
    let moved = change_set
        .files
        .iter()
        .find(|file| file.path == "moved-away.txt")
        .expect("the rename destination");
    assert_eq!(moved.status, TurnFileChangeStatus::Renamed);
    assert_eq!(moved.old_path.as_deref(), Some("movable.txt"));

    let fallback: Vec<(&str, TurnFileChangeStatus)> = change_set
        .files
        .iter()
        .filter(|file| file.path.starts_with("tiny"))
        .map(|file| (file.path.as_str(), file.status))
        .collect();
    assert_eq!(
        fallback,
        vec![
            ("tiny-elsewhere.txt", TurnFileChangeStatus::Added),
            ("tiny.txt", TurnFileChangeStatus::Deleted),
        ],
        "an unpairable move is a delete plus an add: {change_set:?}"
    );
    let paths: Vec<&str> = change_set
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(
        paths,
        ["moved-away.txt", "tiny-elsewhere.txt", "tiny.txt"],
        "sorted by path"
    );
}

#[tokio::test]
async fn binary_changes_keep_their_status_without_line_counts() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "add a binary").await;
    wait_for_requests(&provider, 1).await;

    let mut binary = vec![0u8; 32];
    binary[1] = 1;
    std::fs::write(fixture.path("image.bin"), &binary).unwrap();

    gate.notify_one();
    settle_queue(&engine, "chat-1", true).await;

    let change_set = captured(&engine, "chat-1").await;
    let entry = change_set
        .files
        .iter()
        .find(|file| file.path == "image.bin")
        .expect("the binary file");
    assert_eq!(entry.status, TurnFileChangeStatus::Added);
    assert!(entry.binary, "binary is flagged: {entry:?}");
    assert_eq!((entry.additions, entry.deletions), (0, 0));
}

#[tokio::test]
async fn a_new_file_and_a_deleted_file_carry_their_statuses() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "add and delete").await;
    wait_for_requests(&provider, 1).await;

    std::fs::write(fixture.path("created.txt"), "made by the turn\n").unwrap();
    std::fs::remove_file(fixture.path("movable.txt")).unwrap();

    gate.notify_one();
    settle_queue(&engine, "chat-1", true).await;

    let change_set = captured(&engine, "chat-1").await;
    let status_of = |path: &str| {
        change_set
            .files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.status)
    };
    assert_eq!(status_of("created.txt"), Some(TurnFileChangeStatus::Added));
    assert_eq!(
        status_of("movable.txt"),
        Some(TurnFileChangeStatus::Deleted)
    );
    assert_eq!(change_set.additions, 1);
    assert_eq!(change_set.deletions, 50);
}

// ---------------------------------------------------------------------------
// Failed and interrupted Turns keep their changes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_turn_keeps_its_changes_as_final() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![ScriptedReply::Failed("provider exploded".into())]);
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "work then fail").await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("partial.txt"), "half done\n").unwrap();
    settle_queue(&engine, "chat-1", false).await;

    let change_set = captured(&engine, "chat-1").await;
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
    assert!(
        change_set
            .files
            .iter()
            .any(|file| file.path == "partial.txt"),
        "a failed Turn keeps its partial work: {change_set:?}"
    );
}

#[tokio::test]
async fn an_interrupted_turn_keeps_its_changes_as_final() {
    let fixture = Fixture::new();
    let observed = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let provider = ScriptedProvider::new(vec![ScriptedReply::Cancelling {
        observed: observed.clone(),
        finish: finish.clone(),
    }]);
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let (_, mut sessions) = common::subscribe(&engine, "chat-1").await;

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "work then stop").await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("partial.txt"), "half done\n").unwrap();

    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": { "kind": "interrupt" },
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .expect("the transport observed the cancellation");
    finish.notify_one();
    common::wait_for_session_status(&mut sessions, "chat-1", "idle").await;

    let change_set = captured(&engine, "chat-1").await;
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
    assert!(
        change_set
            .files
            .iter()
            .any(|file| file.path == "partial.txt"),
        "an interrupted Turn keeps its partial work: {change_set:?}"
    );
}

// ---------------------------------------------------------------------------
// Subagent edits belong to the parent Turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_subagent_edit_lands_in_the_parent_turn_change_set() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        // The parent delegates to a worker.
        ScriptedReply::tool_call(
            "spawn-1",
            "Agent",
            serde_json::json!({
                "subagent_type": "worker",
                "description": "Write the assigned file",
                "prompt": "Write the assigned file",
            }),
        ),
        // The worker writes into the shared working directory (full access,
        // so no approval gate parks it).
        ScriptedReply::tool_call(
            "w-1",
            "write",
            serde_json::json!({ "path": "child.txt", "content": "written by the child\n" }),
        ),
        ScriptedReply::text("child done"),
        ScriptedReply::text("parent done"),
    ]);
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatPermissionMode",
                "chatId": "chat-1",
                "mode": "full-access",
            }),
        )
        .await
        .unwrap();

    run_prompt(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "delegate the write",
    )
    .await;
    settle_queue(&engine, "chat-1", true).await;

    let change_set = captured(&engine, "chat-1").await;
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
    let child = change_set
        .files
        .iter()
        .find(|file| file.path == "child.txt")
        .expect("the child's write is part of the parent's change set");
    assert_eq!(child.status, TurnFileChangeStatus::Added);
}

// ---------------------------------------------------------------------------
// Live stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_watch_streams_live_frames_then_a_final_frame() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_TURN_CHANGE_SET,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchTurnChangeSet did not return a stream");
    };
    // The opening frame is the current state (no Turn yet): nothing to
    // report, so the first frame arrives once the Turn exists.
    run_prompt(&engine, "chat-1", &fixture.repo_path(), "edit live").await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nlive edit\n").unwrap();

    let live = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame: TurnChangeSetReply =
                serde_json::from_value(next_frame(&mut stream).await).unwrap();
            if let TurnChangeSetReply::Captured(change_set) = &frame
                && change_set.phase == TurnChangeSetPhase::Live
                && change_set.files.iter().any(|file| file.path == "README.md")
            {
                return change_set.clone();
            }
        }
    })
    .await
    .expect("a live frame arrives");
    assert_eq!(live.files[0].status, TurnFileChangeStatus::Modified);

    gate.notify_one();
    let final_frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame: TurnChangeSetReply =
                serde_json::from_value(next_frame(&mut stream).await).unwrap();
            if let TurnChangeSetReply::Captured(change_set) = &frame
                && change_set.phase == TurnChangeSetPhase::Final
            {
                return change_set.clone();
            }
        }
    })
    .await
    .expect("a final frame arrives");
    assert_eq!(final_frame.files, live.files);
}

#[tokio::test]
async fn the_final_frame_survives_an_auto_advanced_next_turn() {
    // Two queued Turns: releasing the first settles it while the second is
    // already admitted, so the final frame must be read by the settled
    // message id — the chat's "current Turn" is already the next one.
    let fixture = Fixture::new();
    let first = Arc::new(Notify::new());
    let second = Arc::new(Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(first.clone(), "first done"),
        ScriptedReply::gated(second.clone(), "second done"),
    ]);
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;

    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_TURN_CHANGE_SET,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchTurnChangeSet did not return a stream");
    };

    run_prompt(&engine, "chat-1", &fixture.repo_path(), "first turn").await;
    run_prompt(&engine, "chat-1", &fixture.repo_path(), "second turn").await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nfirst turn edit\n").unwrap();
    first.notify_one();
    // The second Turn is admitted (and replaces the current record) before
    // the final frame is read.
    wait_for_requests(&provider, 2).await;

    let final_frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame: TurnChangeSetReply =
                serde_json::from_value(next_frame(&mut stream).await).unwrap();
            if let TurnChangeSetReply::Captured(change_set) = &frame
                && change_set.phase == TurnChangeSetPhase::Final
                && change_set.files.iter().any(|file| file.path == "README.md")
            {
                return change_set.clone();
            }
        }
    })
    .await
    .expect("the settled Turn's final frame still arrives");
    assert_eq!(final_frame.additions, 1);
    second.notify_one();
}

#[tokio::test]
async fn the_watch_reports_unsupported_and_ends_on_a_non_git_root() {
    let fixture = Fixture::new();
    let plain = TempDir::new().unwrap();
    let engine = fixture.engine(&done_provider());
    register_space(&engine, &plain.path().display().to_string(), "space-1").await;
    create_chat(&engine, "chat-1", "space-1").await;

    let RpcReply::Stream(mut stream) = engine
        .handle(
            methods::WATCH_TURN_CHANGE_SET,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchTurnChangeSet did not return a stream");
    };
    let first: TurnChangeSetReply = serde_json::from_value(next_frame(&mut stream).await).unwrap();
    assert!(
        matches!(first, TurnChangeSetReply::Unsupported { .. }),
        "non-Git is explicit on the stream too"
    );
    let end = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the non-Git stream ends promptly");
    assert!(end.is_none(), "the stream ends after its one frame");
}

// ---------------------------------------------------------------------------
// Persistence (ticket 02): settled Turns freeze durably, survive a restart,
// and never move with later workspace edits.
// ---------------------------------------------------------------------------

/// The barrier every persistence assertion waits on: the Turn terminal
/// event publishes only after queue completion AND the change-set record
/// are durable, while the queue watch alone can settle one await earlier.
async fn await_settled(
    engine: &LocalEngine,
    events: &mut futures::stream::BoxStream<'static, serde_json::Value>,
) -> serde_json::Value {
    settle_queue(engine, "chat-1", true).await;
    next_frame(events).await
}

#[tokio::test]
async fn a_settled_turn_change_set_is_restored_by_message_id_after_a_restart() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();
    let settled = await_settled(&engine, &mut events).await;
    assert_eq!(settled["messageId"], "m-1");

    let before = captured(&engine, "chat-1").await;
    assert_eq!(before.phase, TurnChangeSetPhase::Final);
    assert_eq!(before.message_id, "m-1");
    drop(engine);

    // A fresh engine on the same data dir: the settled Turn's summary comes
    // back from the persisted record, not from any live baseline.
    let engine = fixture.engine(&provider);
    match captured_message(&engine, "chat-1", "m-1").await.unwrap() {
        TurnChangeSetReply::Captured(change_set) => {
            assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
            assert_eq!(change_set.message_id, "m-1");
            assert_eq!(change_set.files, before.files, "the frozen files survive");
            assert_eq!(change_set.additions, before.additions);
            assert_eq!(change_set.deletions, before.deletions);
        }
        TurnChangeSetReply::Unsupported { reason } => {
            panic!("expected a restored change set, got unsupported: {reason}")
        }
    }

    // An unknown message id stays an explicit error, not an empty set.
    let error = captured_message(&engine, "chat-1", "never-ran")
        .await
        .expect_err("no record for that Turn");
    assert!(
        error.to_string().contains("no turn recorded"),
        "the UI soft-matches this phrase: {error}"
    );
}

#[tokio::test]
async fn the_persisted_file_diff_is_immutable_under_later_edits_and_restarts() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();
    await_settled(&engine, &mut events).await;
    drop(engine);

    // Later workspace edits must not pollute the settled Turn's history:
    // the persisted before/after pair is the answer, not the live tree.
    std::fs::write(
        fixture.path("README.md"),
        "hello\nuser rewrote everything\n",
    )
    .unwrap();

    let engine = fixture.engine(&provider);
    let text = file_diff_text(&engine, "chat-1", "m-1", "README.md", &fixture.repo_path())
        .await
        .unwrap();
    assert_eq!(text.old_text.as_deref(), Some("hello\n"));
    assert_eq!(text.new_text.as_deref(), Some("hello\nagent edit\n"));
    assert!(!text.stale, "an immutable pair is never stale");
    assert!(!text.binary);

    // And the same answer after a second restart, with the file changed
    // again: history does not move.
    std::fs::write(fixture.path("README.md"), "hello\nand again\n").unwrap();
    drop(engine);
    let engine = fixture.engine(&provider);
    let again = file_diff_text(&engine, "chat-1", "m-1", "README.md", &fixture.repo_path())
        .await
        .unwrap();
    assert_eq!(again.new_text, text.new_text);
}

#[tokio::test]
async fn a_deleted_file_stays_reviewable_from_the_persisted_record() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "delete a file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::remove_file(fixture.path("movable.txt")).unwrap();
    gate.notify_one();
    await_settled(&engine, &mut events).await;
    drop(engine);

    let engine = fixture.engine(&provider);
    let change_set = match captured_message(&engine, "chat-1", "m-1").await.unwrap() {
        TurnChangeSetReply::Captured(change_set) => change_set,
        TurnChangeSetReply::Unsupported { reason } => panic!("unsupported: {reason}"),
    };
    assert_eq!(
        change_set
            .files
            .iter()
            .find(|file| file.path == "movable.txt")
            .map(|file| file.status),
        Some(TurnFileChangeStatus::Deleted)
    );
    let text = file_diff_text(
        &engine,
        "chat-1",
        "m-1",
        "movable.txt",
        &fixture.repo_path(),
    )
    .await
    .unwrap();
    assert_eq!(text.old_text.as_deref(), Some(movable_body().as_str()));
    assert_eq!(text.new_text, None, "the file is gone on the new side");
}

#[tokio::test]
async fn a_rename_persists_its_pre_move_old_side() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "move a file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    // An unmodified move Git pairs as a rename.
    std::fs::rename(fixture.path("movable.txt"), fixture.path("moved-away.txt")).unwrap();
    gate.notify_one();
    await_settled(&engine, &mut events).await;
    drop(engine);

    let engine = fixture.engine(&provider);
    let change_set = match captured_message(&engine, "chat-1", "m-1").await.unwrap() {
        TurnChangeSetReply::Captured(change_set) => change_set,
        TurnChangeSetReply::Unsupported { reason } => panic!("unsupported: {reason}"),
    };
    let moved = change_set
        .files
        .iter()
        .find(|file| file.path == "moved-away.txt")
        .expect("the rename destination");
    assert_eq!(moved.status, TurnFileChangeStatus::Renamed);
    assert_eq!(moved.old_path.as_deref(), Some("movable.txt"));

    // The old side is the pre-move content, not an invented empty file.
    let text = file_diff_text(
        &engine,
        "chat-1",
        "m-1",
        "moved-away.txt",
        &fixture.repo_path(),
    )
    .await
    .unwrap();
    assert_eq!(text.old_text.as_deref(), Some(movable_body().as_str()));
    assert_eq!(text.new_text.as_deref(), Some(movable_body().as_str()));
}

#[tokio::test]
async fn history_outlives_the_repository_disappearing() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();
    let settled = await_settled(&engine, &mut events).await;
    let frozen_files = settled["changeSet"]["files"].clone();
    assert!(frozen_files.is_array(), "{settled}");
    drop(engine);

    // The working directory stops being a Git work tree entirely: the
    // persisted bytes still answer — history never depended on the repo
    // surviving.
    std::fs::remove_dir_all(fixture.repo_dir.path().join(".git")).unwrap();
    let engine = fixture.engine(&provider);
    let change_set = match captured_message(&engine, "chat-1", "m-1").await.unwrap() {
        TurnChangeSetReply::Captured(change_set) => change_set,
        TurnChangeSetReply::Unsupported { reason } => {
            panic!("a persisted record outranks the non-Git answer: {reason}")
        }
    };
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);
    assert_eq!(
        serde_json::to_value(&change_set.files).unwrap(),
        frozen_files,
        "the frozen files survive the repository"
    );
    let text = file_diff_text(&engine, "chat-1", "m-1", "README.md", &fixture.repo_path())
        .await
        .unwrap();
    assert_eq!(text.new_text.as_deref(), Some("hello\nagent edit\n"));
}

#[tokio::test]
async fn the_terminal_event_carries_the_final_change_set_after_persistence() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();

    let event = await_settled(&engine, &mut events).await;
    assert_eq!(event["messageId"], "m-1");
    let change_set = &event["changeSet"];
    assert!(
        change_set.is_object(),
        "the event carries the final set: {event}"
    );
    assert_eq!(change_set["messageId"], "m-1");
    assert_eq!(change_set["phase"], "final");
    assert_eq!(change_set["files"][0]["path"], "README.md", "{change_set}");

    // The payload implies durability: the record was on disk before the
    // event was published.
    assert!(
        fixture
            .data_dir
            .path()
            .join("turn-changes/chat-1/m-1.json")
            .exists(),
        "the persisted record exists by the time the event arrives"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_change_set_persistence_failure_never_fails_the_turn_or_the_event() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    // Break the turn-changes directory so the durable write cannot land.
    let records = fixture.data_dir.path().join("turn-changes");
    std::fs::create_dir_all(&records).unwrap();
    std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();

    // The Turn still settles cleanly and its event still publishes — the
    // change set is history, never Turn correctness — but the event carries
    // no payload it could not durably back.
    settle_queue(&engine, "chat-1", true).await;
    let event = next_frame(&mut events).await;
    assert_eq!(event["messageId"], "m-1");
    assert!(
        event.get("changeSet").is_none(),
        "no payload without a durable record: {event}"
    );
    let change_set = captured(&engine, "chat-1").await;
    assert_eq!(change_set.phase, TurnChangeSetPhase::Final);

    std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn deleting_the_chat_removes_its_persisted_change_sets() {
    let fixture = Fixture::new();
    let (provider, gate) = gated_provider();
    let engine = fixture.engine(&provider);
    setup(&fixture, &engine).await;
    let mut events = subscribe_events(&engine).await;

    queue_run(
        &engine,
        "chat-1",
        &fixture.repo_path(),
        "m-1",
        "edit the file",
    )
    .await;
    wait_for_requests(&provider, 1).await;
    std::fs::write(fixture.path("README.md"), "hello\nagent edit\n").unwrap();
    gate.notify_one();
    await_settled(&engine, &mut events).await;
    assert!(
        fixture
            .data_dir
            .path()
            .join("turn-changes/chat-1/m-1.json")
            .exists()
    );

    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    assert!(
        !fixture.data_dir.path().join("turn-changes/chat-1").exists(),
        "chat deletion reclaims the chat's change-set history"
    );
}
