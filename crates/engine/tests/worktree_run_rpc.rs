//! Session-worktree isolation (ADR-0038): a chat with a persisted isolation
//! intent materializes a dedicated linked worktree at admission and runs
//! every Turn there — including messages queued behind the creating one and
//! sends that carry no WorktreeSpec of their own. A failed materialization is
//! loud and visible in the transcript (user entry + system Error entry), and
//! it never falls back to the main checkout.

mod common;

use std::path::{Path, PathBuf};

use common::{Fixture, ScriptedProvider, ScriptedReply};
use git2::Repository;
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::json;
use tempfile::TempDir;

// -- fixtures ----------------------------------------------------------------

/// A minimal repo: `main` with README.md, and a `feature` branch whose tip
/// adds feature.txt — so creation-base and branch-reuse semantics have
/// distinguishable tips while the main working tree stays clean.
fn setup_repo(dir: &Path) {
    let repo = Repository::init(dir).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let sig = git2::Signature::now("t", "t@t").unwrap();
    let mut index = repo.index().unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    repo.commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
    let parent = repo
        .find_commit(repo.refname_to_id("refs/heads/main").unwrap())
        .unwrap();
    std::fs::write(dir.join("feature.txt"), "feature work\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("feature.txt")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    repo.commit(
        Some("refs/heads/feature"),
        &sig,
        &sig,
        "feature",
        &tree,
        &[&parent],
    )
    .unwrap();
    // The commit never touches the working tree: drop the leftover so the
    // main checkout stays byte-clean for the isolation assertions.
    std::fs::remove_file(dir.join("feature.txt")).unwrap();
}

fn wt_path(fixture: &Fixture, chat_id: &str) -> PathBuf {
    fixture.data_dir.path().join("worktrees").join(chat_id)
}

/// createChat with the isolation intent pre-stamped (the UI's shape), then
/// full access so the scripted write tool executes instead of parking.
async fn create_worktree_chat(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    repo_path: &str,
    base: &str,
) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createChat",
                "chatId": chat_id,
                "cwd": repo_path,
                "worktree": { "repoPath": repo_path, "base": base },
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                    "reasoning": null,
                },
            }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "setChatPermissionMode",
                "chatId": chat_id,
                "mode": "full-access",
            }),
        )
        .await
        .unwrap();
}

async fn queue_run(
    engine: &holt_engine::LocalEngine,
    chat_id: &str,
    message_id: &str,
    cwd: &str,
    prompt: &str,
    worktree: Option<serde_json::Value>,
) {
    let mut request = json!({
        "prompt": prompt,
        "provider": "openai",
        "model": "openai/gpt-5.4",
        "cwd": cwd,
    });
    if let Some(worktree) = worktree {
        request["worktree"] = worktree;
    }
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": chat_id,
                "command": { "kind": "run", "messageId": message_id, "request": request }
            }),
        )
        .await
        .unwrap();
}

/// One Turn whose scripted tool writes `name` into the run's working
/// directory — the physical proof of where the run executed.
fn write_script(name: &str) -> Vec<ScriptedReply> {
    vec![
        ScriptedReply::ToolCalls(vec![common::tool_call(
            "w0",
            "write",
            json!({ "path": name, "content": "probe" }),
        )]),
        ScriptedReply::text("done"),
    ]
}

async fn wait_drained(engine: &holt_engine::LocalEngine) {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({ "chatId": "chat-1" }))
        .await
        .unwrap()
    else {
        panic!("queue watch");
    };
    loop {
        let frame = common::next_frame(&mut watch).await;
        let idle = frame["pending"] == json!([]) && frame["activeMessageId"].is_null();
        if idle {
            return;
        }
    }
}

async fn wait_for_transcript_text(engine: &holt_engine::LocalEngine, needle: &str) {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_DOC_MESSAGES, json!({ "chatId": "chat-1" }))
        .await
        .unwrap()
    else {
        panic!("transcript watch");
    };
    common::wait_for_transcript_text(&mut watch, needle).await;
}

async fn wait_for_transcript_occurrences(
    engine: &holt_engine::LocalEngine,
    needle: &str,
    count: usize,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snapshot = transcript(engine, "chat-1").await;
        if snapshot.matches(needle).count() >= count {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{snapshot}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn let_driver_settle() {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

async fn chat_row(engine: &holt_engine::LocalEngine, chat_id: &str) -> holt_proto::Chat {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!("chats watch");
    };
    let value = common::next_frame(&mut chats).await;
    let rows: Vec<holt_proto::Chat> = serde_json::from_value(value).unwrap();
    rows.into_iter().find(|row| row.id == chat_id).unwrap()
}

async fn space_row(engine: &holt_engine::LocalEngine, space_id: &str) -> holt_proto::Space {
    let RpcReply::Stream(mut spaces) = engine
        .handle(methods::WATCH_SPACES, json!({}))
        .await
        .unwrap()
    else {
        panic!("spaces watch");
    };
    let value = common::next_frame(&mut spaces).await;
    let rows: Vec<holt_proto::Space> = serde_json::from_value(value).unwrap();
    rows.into_iter().find(|row| row.id == space_id).unwrap()
}

async fn transcript(engine: &holt_engine::LocalEngine, chat_id: &str) -> String {
    common::transcript_snapshot(engine, chat_id)
        .await
        .to_string()
}

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn isolation_persists_across_queued_and_specless_sends() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let mut script = write_script("probe.txt");
    script.extend(write_script("probe2.txt"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "main").await;

    // The second message queues while the first runs, carrying the main
    // checkout's cwd and NO spec of its own: only the persisted intent may
    // decide where it executes.
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "main" })),
    )
    .await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    common::wait_for_requests(&provider, 2).await;
    wait_drained(&engine).await;
    let_driver_settle().await;
    queue_run(&engine, "chat-1", "m2", &repo_path, "second", None).await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    common::wait_for_requests(&provider, 4).await;
    wait_drained(&engine).await;

    let wt = wt_path(&fixture, "chat-1");
    // git_worktree_add checked out the base commit's content.
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "hello\n"
    );
    // Both runs executed — and wrote — inside the worktree (each write
    // Turn is two model requests: the tool round and the reply round).
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(
        std::fs::read_to_string(wt.join("probe.txt")).unwrap(),
        "probe"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("probe2.txt")).unwrap(),
        "probe"
    );
    // The main checkout stayed untouched.
    assert!(!repo_dir.path().join("probe.txt").exists());
    assert!(!repo_dir.path().join("probe2.txt").exists());

    let row = chat_row(&engine, "chat-1").await;
    assert_eq!(row.cwd.as_deref(), Some(wt.display().to_string().as_str()));
    assert_eq!(row.branch.as_deref(), Some("holt/chat-1"));
    assert_eq!(row.space_id.as_deref(), Some("wt-chat-1"));
    let space = space_row(&engine, "wt-chat-1").await;
    assert_eq!(space.path, wt.display().to_string());
    assert_eq!(
        row.checkout_id, space.checkout_id,
        "Changes must match the worktree, not the main checkout"
    );
}

#[tokio::test]
async fn failed_preparation_fails_loudly_and_stays_isolated() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "no-such-base").await;

    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "no-such-base" })),
    )
    .await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    wait_for_transcript_text(&engine, "Session worktree could not be prepared").await;

    // No worktree, no run: the failure happened before execution.
    assert!(!wt_path(&fixture, "chat-1").exists());
    assert!(provider.requests().is_empty());
    let text = transcript(&engine, "chat-1").await;
    assert!(
        text.contains("Session worktree could not be prepared"),
        "{text}"
    );

    // The record survives the restart.
    drop(engine);
    let provider2 = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider2);
    let text = transcript(&engine, "chat-1").await;
    assert!(
        text.contains("Session worktree could not be prepared"),
        "{text}"
    );

    // A spec-less resend still resolves through the persisted intent —
    // and fails again instead of silently running in the main checkout.
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({
                "chatId": "chat-1",
                "command": { "kind": "run", "messageId": "m2", "request": {
                    "prompt": "second",
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                    "cwd": repo_path,
                }}
            }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    wait_for_transcript_occurrences(&engine, "Session worktree could not be prepared", 2).await;
    assert!(!wt_path(&fixture, "chat-1").exists());
    assert!(provider2.requests().is_empty());
    let text = transcript(&engine, "chat-1").await;
    assert!(
        text.matches("Session worktree could not be prepared")
            .count()
            >= 2,
        "{text}"
    );
}

#[tokio::test]
async fn existing_branch_without_worktree_completes_materialization() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    // Half-created state: the creation branch exists, the worktree does not.
    // Its tip is feature, NOT the base the spec names.
    let repo = Repository::open(repo_dir.path()).unwrap();
    let feature_tip = repo.refname_to_id("refs/heads/feature").unwrap();
    {
        let commit = repo.find_commit(feature_tip).unwrap();
        repo.branch("holt/chat-1", &commit, false).unwrap();
    }
    drop(repo);

    let provider = ScriptedProvider::new(write_script("probe.txt"));
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "main").await;
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "main" })),
    )
    .await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    common::wait_for_requests(&provider, 2).await;
    wait_drained(&engine).await;

    // The branch was reused — the base never re-applied — and the run
    // executed inside the completed worktree.
    let wt = wt_path(&fixture, "chat-1");
    assert_eq!(
        std::fs::read_to_string(wt.join("feature.txt")).unwrap(),
        "feature work\n"
    );
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(
        std::fs::read_to_string(wt.join("probe.txt")).unwrap(),
        "probe"
    );
    assert!(!repo_dir.path().join("probe.txt").exists());
}

#[tokio::test]
async fn foreign_directory_at_worktree_path_fails_and_keeps_files() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let occupied = wt_path(&fixture, "chat-1");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("junk.txt"), "keep me").unwrap();

    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "main").await;
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "main" })),
    )
    .await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    wait_for_transcript_text(&engine, "exists but is not a usable work tree").await;

    // Loud failure; the foreign files are never touched or deleted.
    assert!(provider.requests().is_empty());
    assert_eq!(
        std::fs::read_to_string(occupied.join("junk.txt")).unwrap(),
        "keep me"
    );
    let text = transcript(&engine, "chat-1").await;
    assert!(
        text.contains("exists but is not a usable work tree"),
        "{text}"
    );
}

#[tokio::test]
async fn worktree_space_conflict_fails_visibly_without_reparenting_chat() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createSpace",
                "spaceId": "wt-chat-1",
                "deviceId": "another-device",
                "path": repo_path,
                "gitDetected": true,
            }),
        )
        .await
        .unwrap();
    create_worktree_chat(&engine, "chat-1", &repo_path, "main").await;
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "main" })),
    )
    .await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    wait_for_transcript_text(&engine, "worktree space wt-chat-1 is bound to another path").await;

    assert!(provider.requests().is_empty());
    let row = chat_row(&engine, "chat-1").await;
    assert_eq!(row.cwd.as_deref(), Some(repo_path.as_str()));
    assert_eq!(row.space_id, None);
    assert_eq!(row.branch, None);
    assert!(wt_path(&fixture, "chat-1").exists());
}

#[tokio::test]
async fn worktree_reuse_ignores_the_creation_branch() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let mut script = write_script("probe.txt");
    script.extend(write_script("probe2.txt"));
    let provider = ScriptedProvider::new(script);
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "main").await;
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first",
        Some(json!({ "repoPath": repo_path, "base": "main" })),
    )
    .await;
    common::wait_for_requests(&provider, 2).await;
    wait_drained(&engine).await;
    let_driver_settle().await;

    // The user switches the worktree to feature and deletes the creation
    // branch — a valid registered worktree must still be reused as-is.
    let wt = wt_path(&fixture, "chat-1");
    let wt_repo = Repository::open(&wt).unwrap();
    wt_repo.set_head("refs/heads/feature").unwrap();
    let main_repo = Repository::open(repo_dir.path()).unwrap();
    main_repo
        .find_branch("holt/chat-1", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    queue_run(&engine, "chat-1", "m2", &repo_path, "second", None).await;
    engine
        .handle(
            methods::CONTINUE_MESSAGE_QUEUE,
            json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    common::wait_for_requests(&provider, 4).await;
    wait_drained(&engine).await;

    assert_eq!(provider.requests().len(), 4);
    assert_eq!(
        std::fs::read_to_string(wt.join("probe2.txt")).unwrap(),
        "probe"
    );
    let row = chat_row(&engine, "chat-1").await;
    assert_eq!(row.branch.as_deref(), Some("feature"));
}

#[tokio::test]
async fn editing_a_never_ran_message_is_transcript_only() {
    let fixture = Fixture::new();
    let repo_dir = TempDir::new().unwrap();
    setup_repo(repo_dir.path());
    let repo_path = repo_dir.path().display().to_string();

    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    create_worktree_chat(&engine, "chat-1", &repo_path, "no-such-base").await;
    queue_run(
        &engine,
        "chat-1",
        "m1",
        &repo_path,
        "first draft",
        Some(json!({ "repoPath": repo_path, "base": "no-such-base" })),
    )
    .await;
    wait_drained(&engine).await;

    // The user entry has no History counterpart (the Turn never ran), so
    // the edit must rewrite the transcript instead of erroring.
    engine
        .handle(
            methods::EDIT_LAST_MESSAGE,
            json!({ "chatId": "chat-1", "messageId": "m1", "prompt": "edited" }),
        )
        .await
        .unwrap();
    let text = transcript(&engine, "chat-1").await;
    assert!(text.contains("edited"), "{text}");
    // The edited message has not run yet: still no model request.
    assert!(provider.requests().is_empty());
}
