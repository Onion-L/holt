//! The review slice (P3): reviewer chats over a worktree's branch diff —
//! fresh-context, read-only, with `submit_review` as the structured
//! submission. Real git repositories and scripted model replies, driven
//! through the `RpcService` trait.

use crate::common;
use common::{ScriptedProvider, ScriptedReply};

use git2::Repository;
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use tempfile::TempDir;

/// main: `root` ← `feature` (one commit ahead, adding feature.txt); the
/// main checkout clean on main; `wt-feature` on the feature branch.
fn scene() -> (TempDir, TempDir, TempDir) {
    let repo_dir = TempDir::new().unwrap();
    let wt_dir = TempDir::new().unwrap();
    let data_dir = TempDir::new().unwrap();
    let repo = common::init_repo(repo_dir.path());
    repo.set_head("refs/heads/main").unwrap();
    let root = common::commit_on(
        &repo,
        "refs/heads/main",
        None,
        &[("README.md", "hello\n")],
        "root",
    );
    {
        let feature = repo.find_commit(root).unwrap();
        repo.branch("feature", &feature, false).unwrap();
    }
    let reference = repo.find_reference("refs/heads/feature").unwrap();
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(&reference));
    let wt_path = wt_dir.path().join("wt-feature");
    repo.worktree("wt-feature", &wt_path, Some(&options))
        .unwrap();
    drop(reference);
    {
        let worktree_repo = Repository::open(wt_dir.path().join("wt-feature")).unwrap();
        common::commit_on(
            &worktree_repo,
            "refs/heads/feature",
            Some(root),
            &[("README.md", "hello\n"), ("feature.txt", "feature work\n")],
            "feature work",
        );
        // commit_on never touches the working tree: sync so the worktree
        // is clean at its branch head.
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        worktree_repo.checkout_head(Some(&mut checkout)).unwrap();
    }
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout)).unwrap();
    drop(repo);
    (repo_dir, wt_dir, data_dir)
}

fn assemble(data_dir: &TempDir, provider: &ScriptedProvider) -> LocalEngine {
    LocalEngine::assemble(&EngineConfig {
        data_dir: data_dir.path().to_path_buf(),
        personal_skills_dir: None,
        stream_fn: Some(provider.stream_fn()),
        search_backend_resolver: None,
        clock: None,
        exec_shell: None,
    })
    .unwrap()
}

async fn call(
    engine: &LocalEngine,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, holt_rpc::RpcError> {
    match engine.handle(method, params).await {
        Ok(RpcReply::Value(value)) => Ok(value),
        Ok(_) => panic!("{method} did not return a value"),
        Err(error) => Err(error),
    }
}

/// Commit `.holt/reviewers/<name>.md` onto the trunk.
fn commit_repo_reviewer(repo_dir: &TempDir, name: &str, body: &str) {
    commit_reviewer_on(repo_dir, "refs/heads/main", name, body);
}

/// Commit `.holt/reviewers/<name>.md` onto `refname`.
fn commit_reviewer_on(repo_dir: &TempDir, refname: &str, name: &str, body: &str) {
    let repo = Repository::open(repo_dir.path()).unwrap();
    let tip_commit = repo
        .find_reference(refname)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let blob = repo.blob(body.as_bytes()).unwrap();
    // The reviewers live in `.holt/reviewers/` — reuse the existing
    // `reviewers` subtree when present, else graft a fresh one into a
    // fresh `.holt`.
    let reviewers_tree = 'block: {
        let root = tip_commit.tree().unwrap();
        if let Ok(holt) = root.get_path(std::path::Path::new(".holt")) {
            let holt_tree = repo.find_tree(holt.id()).unwrap();
            if let Ok(existing) = holt_tree.get_path(std::path::Path::new("reviewers")) {
                let mut builder = repo
                    .treebuilder(Some(&repo.find_tree(existing.id()).unwrap()))
                    .unwrap();
                builder
                    .insert(format!("{name}.md"), blob, 0o100644)
                    .unwrap();
                break 'block builder.write().unwrap();
            }
        }
        let mut builder = repo.treebuilder(None).unwrap();
        builder
            .insert(format!("{name}.md"), blob, 0o100644)
            .unwrap();
        builder.write().unwrap()
    };
    let reviewers = {
        let root = tip_commit.tree().unwrap();
        let mut holt_builder = match root.get_path(std::path::Path::new(".holt")) {
            Ok(holt) => repo
                .treebuilder(Some(&repo.find_tree(holt.id()).unwrap()))
                .unwrap(),
            Err(_) => repo.treebuilder(None).unwrap(),
        };
        holt_builder
            .insert("reviewers", reviewers_tree, 0o040000)
            .unwrap();
        holt_builder.write().unwrap()
    };
    let mut root_builder = repo.treebuilder(Some(&tip_commit.tree().unwrap())).unwrap();
    root_builder.insert(".holt", reviewers, 0o040000).unwrap();
    let tree = repo.find_tree(root_builder.write().unwrap()).unwrap();
    let sig = common::signature();
    repo.commit(
        Some(refname),
        &sig,
        &sig,
        "declare a reviewer",
        &tree,
        &[&tip_commit],
    )
    .unwrap();
}

async fn chats_of(engine: &LocalEngine) -> Vec<holt_proto::Chat> {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!()
    };
    let value = common::next_frame(&mut chats).await;
    serde_json::from_value(value).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_turn_is_read_only_and_mounts_submit_review() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    // A configured MCP server: an ordinary Turn would mount its tools.
    write_mcp_config(&data_dir);
    // The scripted model tries `write` (absent — the error result proves
    // the read-only set), then submits a clean review. A plain text reply
    // would end the Turn, so the script never uses one.
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call(
            "try-write",
            "write",
            serde_json::json!({ "path": "forbidden.txt", "content": "no" }),
        ),
        ScriptedReply::tool_call(
            "submit",
            "submit_review",
            serde_json::json!({ "verdict": "clean" }),
        ),
    ]);
    let engine = assemble(&data_dir, &provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();

    let started = call(
        &engine,
        methods::START_REVIEW,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
            "reviewer": "bugbot",
            "model": "openai/gpt-5.4",
        }),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "idle").await;
    // The chat row carries review state.
    let chats = chats_of(&engine).await;
    let row = chats.iter().find(|chat| chat.id == chat_id).unwrap();
    let review = row.review.as_ref().unwrap();
    assert_eq!(review.reviewer, "bugbot");
    assert_eq!(review.worktree, "wt-feature");
    assert!(!review.trunk_head.is_empty());
    assert!(!review.branch_head.is_empty());
    assert!(!review.trunk_head.is_empty() && review.trunk_head != review.branch_head);

    // The review landed on the row.
    assert_eq!(review.verdict.as_deref(), Some("clean"));
    assert!(review.findings.as_ref().is_some_and(Vec::is_empty));

    // The forbidden write never happened (the tool was absent), and the
    // worktree is untouched.
    let worktree_root = _wt_dir.path().join("wt-feature");
    assert!(!worktree_root.join("forbidden.txt").exists());

    // Exactly the read-only set plus submit_review — no bash/edit/write,
    // no delegation, no MCP.
    let requests = provider.requests();
    let mut names = requests[0].tool_names.clone();
    names.sort();
    assert_eq!(
        names,
        [
            "grep",
            "ls",
            "read",
            "read_chat",
            "submit_review",
            "web_fetch",
            "web_search"
        ],
        "the review toolset"
    );
    // The submission ended the Turn: the write attempt's error cost one
    // follow-up request, the submit none.
    assert_eq!(requests.len(), 2, "no request after submit_review");
}

/// `mcp.json` pointing at the stdio fixture server.
fn write_mcp_config(data_dir: &TempDir) {
    std::fs::write(
        data_dir.path().join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "fixture": { "command": env!("CARGO_BIN_EXE_mcp_stdio_fixture") }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_message_carries_the_diff_not_the_conversation() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let provider = ScriptedProvider::new(vec![ScriptedReply::tool_call(
        "submit",
        "submit_review",
        serde_json::json!({
            "verdict": "issues",
            "findings": [{
                "severity": "high",
                "path": "feature.txt",
                "line": 1,
                "title": "the feature is wrong",
                "detail": "evidence: `feature work`",
                "origin": "introduced",
            }],
        }),
    )]);
    let engine = assemble(&data_dir, &provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();

    // An author chat exists with conversation content that must NOT ride
    // along.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "wt-feature",
                "spaceId": "home",
                "cwd": _wt_dir.path().join("wt-feature").display().to_string(),
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                },
            }),
        )
        .await
        .unwrap();

    let started = call(
        &engine,
        methods::START_REVIEW,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
            "reviewer": "bugbot",
        }),
    )
    .await
    .unwrap();
    common::wait_for_chat_status(&engine, started["chatId"].as_str().unwrap(), "idle").await;
    common::wait_for_requests(&provider, 1).await;

    let request = &provider.requests()[0];
    let body = format!(
        "{:?}\n{}",
        request.messages,
        request.system_prompt.as_deref().unwrap_or_default()
    );
    assert!(
        body.contains("feature.txt") && body.contains("feature work"),
        "the diff rides the first message"
    );

    // The findings landed with typed shape.
    let chats = chats_of(&engine).await;
    let row = chats
        .iter()
        .find(|chat| chat.id == started["chatId"].as_str().unwrap())
        .unwrap();
    let review = row.review.as_ref().unwrap();
    assert_eq!(review.verdict.as_deref(), Some("issues"));
    let findings = review.findings.as_ref().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, "high");
    assert_eq!(findings[0].path, "feature.txt");
    assert_eq!(findings[0].origin, "introduced");
    assert!(!findings[0].id.is_empty(), "ids are minted engine-side");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_reviewers_extend_and_override_the_catalog() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    commit_repo_reviewer(&repo_dir, "perf", "You review for performance only.");
    commit_repo_reviewer(&repo_dir, "bugbot", "The repo's own bugbot.");
    let engine = assemble(&data_dir, &ScriptedProvider::new(vec![]));

    let listed = call(
        &engine,
        methods::LIST_REVIEWERS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    let rows = listed.as_array().unwrap();
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {rows:?}"))
    };
    assert_eq!(by_name("bugbot")["source"], "repo", "a repo file shadows");
    assert_eq!(by_name("security")["source"], "builtin");
    assert_eq!(by_name("perf")["source"], "repo", "a repo reviewer lists");

    // The override text is what a started review runs with.
    drop(engine);
    let provider = ScriptedProvider::new(vec![ScriptedReply::tool_call(
        "submit",
        "submit_review",
        serde_json::json!({ "verdict": "clean" }),
    )]);
    let engine = assemble(&data_dir, &provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    let started = call(
        &engine,
        methods::START_REVIEW,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
            "reviewer": "bugbot",
            "model": "openai/gpt-5.4",
        }),
    )
    .await
    .unwrap();
    common::wait_for_chat_status(&engine, started["chatId"].as_str().unwrap(), "idle").await;
    common::wait_for_requests(&provider, 1).await;
    let request = format!("{:?}", provider.requests()[0].system_prompt);
    assert!(
        request.contains("The repo's own bugbot."),
        "the repo override rode the system prompt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_head_marks_the_review_stale_in_the_list() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let provider = ScriptedProvider::new(vec![ScriptedReply::tool_call(
        "submit",
        "submit_review",
        serde_json::json!({ "verdict": "clean" }),
    )]);
    let engine = assemble(&data_dir, &provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    let started = call(
        &engine,
        methods::START_REVIEW,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
            "reviewer": "bugbot",
            "model": "openai/gpt-5.4",
        }),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "idle").await;

    let listed = call(
        &engine,
        methods::LIST_REVIEWS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["stale"], serde_json::json!(false));

    // The trunk moves: the review reads stale.
    let repo = Repository::open(repo_dir.path()).unwrap();
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
        &[("README.md", "hello\n"), ("later.txt", "later\n")],
        "trunk moves",
    );
    drop(repo);
    let listed = call(
        &engine,
        methods::LIST_REVIEWS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        listed[0]["stale"],
        serde_json::json!(true),
        "a moved trunk marks the review stale"
    );
    assert_eq!(listed[0]["verdict"], serde_json::json!("clean"));
    assert_eq!(listed[0]["findingCount"], serde_json::json!(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sending_findings_back_queues_into_the_author_chat() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let engine = assemble(&data_dir, &ScriptedProvider::new(vec![]));
    // The author chat, with a config the UI's send-back would reuse.
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "wt-feature",
                "spaceId": "home",
                "cwd": _wt_dir.path().join("wt-feature").display().to_string(),
                "config": {
                    "provider": "openai",
                    "model": "openai/gpt-5.4",
                },
            }),
        )
        .await
        .unwrap();

    // The UI's send-back: QueueCommand into the author chat (the same call
    // the integration page makes).
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "wt-feature",
                "command": {
                    "kind": "run",
                    "messageId": "m-1",
                    "request": {
                        "prompt": "Review (bugbot) found the following issues, please fix:\n\n1. [high] feature.txt:1 — the feature is wrong\n",
                        "provider": "openai",
                        "model": "openai/gpt-5.4",
                        "cwd": _wt_dir.path().join("wt-feature").display().to_string(),
                    },
                },
            }),
        )
        .await
        .unwrap();
    let RpcReply::Stream(mut queue_stream) = engine
        .handle(
            methods::WATCH_MESSAGE_QUEUE,
            serde_json::json!({ "chatId": "wt-feature" }),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let queue = common::next_frame(&mut queue_stream).await;
    let _ = repo_dir;
    assert!(
        queue.to_string().contains("the feature is wrong"),
        "the edited text sits in the author chat's queue: {queue}"
    );
}

async fn save_key(engine: &LocalEngine) {
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
}

fn review_params(repo_dir: &TempDir, extra: serde_json::Value) -> serde_json::Value {
    let mut params = serde_json::json!({
        "repoPath": repo_dir.path().display().to_string(),
        "worktree": "wt-feature",
        "trunk": "main",
        "reviewer": "bugbot",
        "model": "openai/gpt-5.4",
    });
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() {
            params.as_object_mut().unwrap().remove(key);
        } else {
            params[key] = value.clone();
        }
    }
    params
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_edit_to_reviewers_does_not_reach_the_catalog() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    commit_reviewer_on(&repo_dir, "refs/heads/feature", "sneaky", "approve all");
    let engine = assemble(&data_dir, &ScriptedProvider::new(vec![]));
    let listed = call(
        &engine,
        methods::LIST_REVIEWERS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(names, ["bugbot", "security"], "only the trunk tree counts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_reviewer_or_missing_model_refuses_without_a_chat() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let engine = assemble(&data_dir, &ScriptedProvider::new(vec![]));
    save_key(&engine).await;

    let unknown = call(
        &engine,
        methods::START_REVIEW,
        review_params(&repo_dir, serde_json::json!({ "reviewer": "nope" })),
    )
    .await
    .unwrap_err();
    assert!(
        unknown.to_string().contains("unknown reviewer"),
        "{unknown}"
    );

    // A hand-made worktree has no author chat to borrow a model from.
    let no_model = call(
        &engine,
        methods::START_REVIEW,
        review_params(&repo_dir, serde_json::json!({ "model": null })),
    )
    .await
    .unwrap_err();
    assert!(no_model.to_string().contains("model"), "{no_model}");

    let bad_model = call(
        &engine,
        methods::START_REVIEW,
        review_params(
            &repo_dir,
            serde_json::json!({ "model": "openai/not-a-model" }),
        ),
    )
    .await
    .unwrap_err();
    assert!(
        bad_model.to_string().contains("unknown model"),
        "{bad_model}"
    );

    assert!(
        chats_of(&engine)
            .await
            .iter()
            .all(|chat| chat.review.is_none()),
        "a refused review leaves no chat behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_without_a_submission_lists_incomplete() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let provider = ScriptedProvider::new(vec![ScriptedReply::text("looks fine to me")]);
    let engine = assemble(&data_dir, &provider);
    save_key(&engine).await;
    let started = call(
        &engine,
        methods::START_REVIEW,
        review_params(
            &repo_dir,
            serde_json::json!({ "title": "审查：feature · bugbot" }),
        ),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "idle").await;

    let listed = call(
        &engine,
        methods::LIST_REVIEWS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["state"], "incomplete");
    assert!(listed[0]["verdict"].is_null());

    let chats = chats_of(&engine).await;
    let row = chats.iter().find(|chat| chat.id == chat_id).unwrap();
    assert_eq!(row.title.as_deref(), Some("审查：feature · bugbot"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_title_names_the_branch() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let provider = ScriptedProvider::new(vec![ScriptedReply::tool_call(
        "submit",
        "submit_review",
        serde_json::json!({ "verdict": "clean" }),
    )]);
    let engine = assemble(&data_dir, &provider);
    save_key(&engine).await;
    let started = call(
        &engine,
        methods::START_REVIEW,
        review_params(&repo_dir, serde_json::json!({})),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "idle").await;
    let chats = chats_of(&engine).await;
    let row = chats.iter().find(|chat| chat.id == chat_id).unwrap();
    assert_eq!(row.title.as_deref(), Some("Review: feature · bugbot"));
    let listed = call(
        &engine,
        methods::LIST_REVIEWS,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
        }),
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["state"], "submitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_the_worktree_rehomes_its_review_chats() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    let provider = ScriptedProvider::new(vec![ScriptedReply::tool_call(
        "submit",
        "submit_review",
        serde_json::json!({ "verdict": "clean" }),
    )]);
    let engine = assemble(&data_dir, &provider);
    save_key(&engine).await;
    let started = call(
        &engine,
        methods::START_REVIEW,
        review_params(&repo_dir, serde_json::json!({})),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "idle").await;
    let review_space = chats_of(&engine)
        .await
        .into_iter()
        .find(|chat| chat.id == chat_id)
        .and_then(|chat| chat.space_id)
        .unwrap();
    assert!(review_space.starts_with("review-"), "{review_space}");

    // The driver clears its running flag a scheduler tick after the idle
    // status lands; the delete is accepted once it has. Bounded, and only
    // for this settling refusal — anything else fails the test.
    let mut deleted = false;
    for _ in 0..100 {
        match call(
            &engine,
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": repo_dir.path().display().to_string(),
                "worktree": "wt-feature",
                "force": false,
            }),
        )
        .await
        {
            Ok(_) => {
                deleted = true;
                break;
            }
            Err(error) if error.to_string().contains("stop it first") => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => panic!("unexpected delete failure: {error}"),
        }
    }
    assert!(deleted, "a settled review must never block the delete");

    let chats = chats_of(&engine).await;
    let row = chats.iter().find(|chat| chat.id == chat_id).unwrap();
    assert_ne!(row.space_id.as_deref(), Some(review_space.as_str()));
    let RpcReply::Stream(mut spaces) = engine
        .handle(methods::WATCH_SPACES, serde_json::json!({}))
        .await
        .unwrap()
    else {
        panic!()
    };
    let spaces: Vec<holt_proto::Space> =
        serde_json::from_value(common::next_frame(&mut spaces).await).unwrap();
    assert!(
        spaces
            .iter()
            .any(|space| Some(&space.id) == row.space_id.as_ref()),
        "the review chat points at a live Space"
    );
    assert!(!spaces.iter().any(|space| space.id == review_space));
}

#[tokio::test]
async fn delete_refuses_while_a_review_turn_runs_in_the_worktree() {
    let (repo_dir, _wt_dir, data_dir) = scene();
    // The reviewer never answers: the review Turn hangs open.
    let provider = ScriptedProvider::new(vec![ScriptedReply::Silent]);
    let engine = assemble(&data_dir, &provider);
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    let started = call(
        &engine,
        methods::START_REVIEW,
        serde_json::json!({
            "repoPath": repo_dir.path().display().to_string(),
            "worktree": "wt-feature",
            "trunk": "main",
            "reviewer": "bugbot",
            "model": "openai/gpt-5.4",
        }),
    )
    .await
    .unwrap();
    let chat_id = started["chatId"].as_str().unwrap().to_string();
    common::wait_for_chat_status(&engine, &chat_id, "working").await;

    // The review chat is not the owning chat (a different id), but its Turn
    // runs against the worktree's directory — the delete must refuse.
    assert_ne!(chat_id, "wt-feature");
    let error = match engine
        .handle(
            methods::DELETE_WORKTREE,
            serde_json::json!({
                "repoPath": repo_dir.path().display().to_string(),
                "worktree": "wt-feature",
                "force": true,
            }),
        )
        .await
    {
        Ok(_) => panic!("a running review Turn must refuse the delete"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stop it first"), "got: {error}");
    let repo = Repository::open(repo_dir.path()).unwrap();
    let names: Vec<String> = repo
        .worktrees()
        .unwrap()
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    assert!(names.contains(&"wt-feature".to_string()), "nothing removed");
}
