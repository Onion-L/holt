//! `SearchFiles` over the handle seam: root resolution from chat/space
//! state, the walker contract (hidden in, gitignore out of the way, `.git`
//! never descended), fuzzy scoring on the full relative path, and the
//! bounded, deterministic result list.

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, ScriptedProvider};
use holt_engine::LocalEngine;
use holt_proto::FileSearchMatch;
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::json;

/// A workspace tree exercising every walker rule: nested dirs, a hidden
/// file and dir, a gitignored file, and a `.git` directory that must never
/// be searched.
fn build_tree(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src/lib.rs"), "// lib\n").unwrap();
    fs::create_dir_all(root.join("composer")).unwrap();
    fs::write(root.join("composer/send.rs"), "// send\n").unwrap();
    fs::write(root.join("composer/receive.rs"), "// receive\n").unwrap();
    fs::write(root.join(".hidden_file"), "secret\n").unwrap();
    fs::create_dir_all(root.join(".hidden_dir")).unwrap();
    fs::write(root.join(".hidden_dir/secret.txt"), "secret\n").unwrap();
    fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
    fs::write(root.join("ignored.log"), "noise\n").unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    fs::write(root.join(".git/refs/heads/main"), "deadbeef\n").unwrap();
    // A worktree/submodule carries `.git` as a gitdir-pointer FILE — it must
    // be excluded just like the directory.
    fs::create_dir_all(root.join("worktree")).unwrap();
    fs::write(root.join("worktree/.git"), "gitdir: /elsewhere\n").unwrap();
    fs::write(root.join("worktree/tracked.rs"), "// tracked\n").unwrap();
}

async fn setup(fixture: &Fixture) -> LocalEngine {
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createSpace",
                "spaceId": "space-1",
                "deviceId": engine.engine_info().device_id,
                "path": fixture.cwd(),
            }),
        )
        .await
        .unwrap();
    engine
}

async fn create_chat(engine: &LocalEngine, chat_id: &str, params: serde_json::Value) {
    let mut op = json!({ "op": "createChat", "chatId": chat_id });
    op.as_object_mut()
        .unwrap()
        .extend(params.as_object().unwrap().clone());
    engine.handle(methods::MUTATE, op).await.unwrap();
}

async fn search(engine: &LocalEngine, params: serde_json::Value) -> Vec<FileSearchMatch> {
    try_search(engine, params).await.unwrap()
}

async fn try_search(
    engine: &LocalEngine,
    params: serde_json::Value,
) -> Result<Vec<FileSearchMatch>, RpcError> {
    match engine.handle(methods::SEARCH_FILES, params).await? {
        RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
        _ => panic!("SearchFiles must reply with a value"),
    }
}

fn paths(matches: &[FileSearchMatch]) -> Vec<&str> {
    matches.iter().map(|hit| hit.path.as_str()).collect()
}

mod search_files {
    use super::*;

    #[tokio::test]
    async fn resolves_the_root_by_chat_id_and_by_space_id() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;
        create_chat(&engine, "chat-1", json!({ "spaceId": "space-1" })).await;

        let by_chat = search(&engine, json!({ "query": "send", "chatId": "chat-1" })).await;
        let by_space = search(&engine, json!({ "query": "send", "spaceId": "space-1" })).await;
        assert!(!by_chat.is_empty());
        assert_eq!(by_chat, by_space);
        assert!(paths(&by_chat).contains(&"composer/send.rs"));
    }

    #[tokio::test]
    async fn unknown_chat_or_space_fails() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let chat = try_search(&engine, json!({ "query": "send", "chatId": "nope" })).await;
        assert!(matches!(chat, Err(RpcError::Failed(_))), "chat: {chat:?}");
        let space = try_search(&engine, json!({ "query": "send", "spaceId": "nope" })).await;
        assert!(
            matches!(space, Err(RpcError::Failed(_))),
            "space: {space:?}"
        );
    }

    #[tokio::test]
    async fn missing_or_ambiguous_root_selector_is_bad_params() {
        let fixture = Fixture::new();
        let engine = setup(&fixture).await;

        let neither = try_search(&engine, json!({ "query": "send" })).await;
        assert!(
            matches!(neither, Err(RpcError::BadParams(_))),
            "neither: {neither:?}"
        );
        let both = try_search(
            &engine,
            json!({ "query": "send", "chatId": "c", "spaceId": "s" }),
        )
        .await;
        assert!(
            matches!(both, Err(RpcError::BadParams(_))),
            "both: {both:?}"
        );
        let malformed = try_search(&engine, json!("not an object")).await;
        assert!(
            matches!(malformed, Err(RpcError::BadParams(_))),
            "malformed: {malformed:?}"
        );
    }

    #[tokio::test]
    async fn files_and_directories_are_returned_with_is_dir() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let matches = search(
            &engine,
            json!({ "query": "composer", "spaceId": "space-1" }),
        )
        .await;
        let dir = matches
            .iter()
            .find(|hit| hit.path == "composer")
            .expect("the composer directory is a candidate");
        assert!(dir.is_dir);
        let file = matches
            .iter()
            .find(|hit| hit.path == "composer/send.rs")
            .expect("files under a matching dir are candidates");
        assert!(!file.is_dir);
    }

    #[tokio::test]
    async fn fuzzy_matching_runs_against_the_full_relative_path() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let matches = search(
            &engine,
            json!({ "query": "cmp/send", "spaceId": "space-1" }),
        )
        .await;
        assert_eq!(
            paths(&matches).first(),
            Some(&"composer/send.rs"),
            "matches: {matches:?}"
        );
    }

    #[tokio::test]
    async fn hidden_and_gitignored_files_are_candidates_dot_git_is_never_searched() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let hidden = search(&engine, json!({ "query": "hidden", "spaceId": "space-1" })).await;
        let hidden_paths = paths(&hidden);
        assert!(hidden_paths.contains(&".hidden_file"), "{hidden:?}");
        assert!(
            hidden_paths.contains(&".hidden_dir/secret.txt"),
            "{hidden:?}"
        );

        let ignored = search(&engine, json!({ "query": "ignored", "spaceId": "space-1" })).await;
        assert!(paths(&ignored).contains(&"ignored.log"), "{ignored:?}");

        // `.git/refs/heads/main` would match `refs` if the walker descended.
        let refs = search(&engine, json!({ "query": "refs", "spaceId": "space-1" })).await;
        assert!(
            refs.iter().all(|hit| !hit.path.starts_with(".git")),
            "{refs:?}"
        );

        // The worktree's gitdir-pointer `.git` FILE stays out too, while the
        // worktree's real files remain searchable.
        let worktree = search(
            &engine,
            json!({ "query": "worktree", "spaceId": "space-1" }),
        )
        .await;
        let worktree_paths = paths(&worktree);
        assert!(
            worktree_paths.contains(&"worktree/tracked.rs"),
            "{worktree:?}"
        );
        assert!(
            worktree_paths.iter().all(|path| !path.ends_with(".git")),
            "{worktree:?}"
        );
    }

    #[tokio::test]
    async fn results_are_bounded_and_stably_ordered() {
        let fixture = Fixture::new();
        let root = fixture.project_dir.path();
        fs::create_dir_all(root.join("many")).unwrap();
        for index in 0..70 {
            fs::write(root.join("many").join(format!("file{index:02}.txt")), "x\n").unwrap();
        }
        let engine = setup(&fixture).await;

        let params = || json!({ "query": "many/file", "spaceId": "space-1" });
        let first = search(&engine, params()).await;
        let second = search(&engine, params()).await;
        assert_eq!(first.len(), 50, "the result list is capped at the limit");
        assert_eq!(first, second, "the same query orders identically");
    }

    #[tokio::test]
    async fn an_empty_query_returns_an_empty_list() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        for query in ["", "   "] {
            let matches = search(&engine, json!({ "query": query, "spaceId": "space-1" })).await;
            assert!(matches.is_empty(), "query {query:?}: {matches:?}");
        }
    }

    #[tokio::test]
    async fn a_chat_cwd_takes_priority_over_its_space_path() {
        let fixture = Fixture::new();
        build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;
        let cwd = fixture.project_dir.path().join("composer");
        create_chat(
            &engine,
            "chat-1",
            json!({ "spaceId": "space-1", "cwd": cwd.display().to_string() }),
        )
        .await;

        let matches = search(&engine, json!({ "query": "send", "chatId": "chat-1" })).await;
        assert!(
            paths(&matches).contains(&"send.rs"),
            "paths are relative to the chat cwd: {matches:?}"
        );
    }
}
