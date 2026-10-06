//! Routines (ADR-0042) over the RPC surface: create, list, delete, the
//! Routines watch, and Run now — the run Chat's shape (the Routine's name as
//! a user-owned title, its model and Permission mode, the run marker, the
//! prompt verbatim as the first message), and persistence across restart.

mod common;

use std::path::Path;

use chrono::{TimeZone, Utc};
use common::{Fixture, ScriptedProvider, ScriptedReply, next_frame, wait_for_requests};
use holt_engine::{Clock, LocalEngine};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::{Value, json};

const PROMPT: &str = "Summarize yesterday's commits.";

fn clock() -> Clock {
    Clock::manual(Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap())
}

async fn setup(fixture: &Fixture, provider: &ScriptedProvider) -> LocalEngine {
    let engine = fixture.engine_with_clock(provider, clock());
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

fn create_params(extra: Value) -> Value {
    let mut params = json!({
        "name": "Daily digest",
        "spaceId": "space-1",
        "prompt": PROMPT,
        "cron": "0 9 * * *",
        "timeZone": "Asia/Shanghai",
        "config": {
            "provider": "openai",
            "model": "openai/gpt-5.4",
            "reasoning": null,
            "permissionMode": "auto-review",
        },
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    params
}

async fn create(engine: &LocalEngine, extra: Value) -> Value {
    let RpcReply::Value(routine) = engine
        .handle(methods::CREATE_ROUTINE, create_params(extra))
        .await
        .unwrap()
    else {
        panic!("CreateRoutine did not reply a value");
    };
    routine
}

async fn list(engine: &LocalEngine) -> Vec<Value> {
    let RpcReply::Value(value) = engine
        .handle(methods::LIST_ROUTINES, json!({}))
        .await
        .unwrap()
    else {
        panic!("ListRoutines did not reply a value");
    };
    value.as_array().unwrap().clone()
}

async fn run_now(engine: &LocalEngine, routine_id: &str) -> String {
    let RpcReply::Value(value) = engine
        .handle(methods::RUN_ROUTINE_NOW, json!({ "routineId": routine_id }))
        .await
        .unwrap()
    else {
        panic!("RunRoutineNow did not reply a value");
    };
    value["chatId"].as_str().unwrap().to_string()
}

async fn chat_row(engine: &LocalEngine, chat_id: &str) -> Option<Value> {
    let RpcReply::Stream(mut chats) = engine
        .handle(methods::WATCH_CHATS, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchChats did not return a stream");
    };
    next_frame(&mut chats)
        .await
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == chat_id)
        .cloned()
}

fn provider() -> ScriptedProvider {
    ScriptedProvider::new(vec![]).with_chat_script(PROMPT, vec![ScriptedReply::text("Done.")])
}

#[tokio::test]
async fn create_list_and_delete() {
    let fixture = Fixture::new();
    let provider = provider();
    let engine = setup(&fixture, &provider).await;
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_ROUTINES, json!({}))
        .await
        .unwrap()
    else {
        panic!("WatchRoutines did not return a stream");
    };
    assert_eq!(next_frame(&mut watch).await, json!([]));

    let routine = create(&engine, json!({})).await;
    assert_eq!(routine["name"], "Daily digest");
    assert_eq!(routine["timeZone"], "Asia/Shanghai");
    assert_eq!(routine["checkout"], "main-checkout");
    assert_eq!(routine["createdAt"], "2026-10-07T09:00:00Z");
    assert_eq!(routine["runs"], json!([]));
    let id = routine["id"].as_str().unwrap();

    let frame = next_frame(&mut watch).await;
    assert_eq!(frame[0]["id"], id);
    let listed = list(&engine).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["prompt"], PROMPT);
    assert_eq!(listed[0]["config"]["permissionMode"], "auto-review");

    engine
        .handle(methods::DELETE_ROUTINE, json!({ "routineId": id }))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut watch).await, json!([]));
    assert!(list(&engine).await.is_empty());
}

#[tokio::test]
async fn create_defaults_the_time_zone_and_rejects_bad_params() {
    let fixture = Fixture::new();
    let provider = provider();
    let engine = setup(&fixture, &provider).await;
    let mut params = create_params(json!({}));
    params.as_object_mut().unwrap().remove("timeZone");
    let RpcReply::Value(routine) = engine
        .handle(methods::CREATE_ROUTINE, params)
        .await
        .unwrap()
    else {
        panic!("CreateRoutine did not reply a value");
    };
    assert!(!routine["timeZone"].as_str().unwrap().is_empty());

    for extra in [
        json!({ "name": "  " }),
        json!({ "prompt": "" }),
        json!({ "spaceId": "missing" }),
    ] {
        let error = engine
            .handle(methods::CREATE_ROUTINE, create_params(extra.clone()))
            .await
            .err()
            .unwrap_or_else(|| panic!("{extra} was accepted"));
        assert!(
            matches!(error, RpcError::BadParams(_)),
            "{extra}: {error:?}"
        );
    }
    assert_eq!(list(&engine).await.len(), 1);
}

#[tokio::test]
async fn run_now_starts_a_run_chat_and_records_the_run() {
    let fixture = Fixture::new();
    let provider = provider();
    let engine = setup(&fixture, &provider).await;
    let routine = create(&engine, json!({})).await;
    let id = routine["id"].as_str().unwrap();

    let chat_id = run_now(&engine, id).await;
    wait_for_requests(&provider, 1).await;
    assert!(
        common::summarize(&provider.requests()[0].messages)
            .first()
            .is_some_and(|first| first.contains(PROMPT)),
        "the prompt is the run's first message verbatim"
    );

    let row = chat_row(&engine, &chat_id).await.expect("run chat listed");
    assert_eq!(row["title"], "Daily digest");
    assert_eq!(row["titleSource"], "userManual");
    assert_eq!(row["spaceId"], "space-1");
    assert_eq!(row["cwd"], fixture.cwd());
    assert_eq!(row["config"]["model"], "openai/gpt-5.4");
    assert_eq!(row["config"]["permissionMode"], "auto-review");
    assert_eq!(row["createdAt"], "2026-10-07T09:00:00Z");
    assert!(row.get("worktree").is_none());
    assert_eq!(
        row["routineRun"],
        json!({
            "routineId": id,
            "routineName": "Daily digest",
            "missedFires": 0,
            "manual": true,
        })
    );

    let listed = list(&engine).await;
    let run = &listed[0]["runs"][0];
    assert_eq!(run["chatId"], chat_id);
    assert_eq!(run["manual"], true);
    assert_eq!(run["firedAt"], "2026-10-07T09:00:00Z");
    // Run now is off schedule: the schedule's anchor does not move.
    assert!(listed[0].get("lastFiredAt").is_none_or(Value::is_null));
}

#[tokio::test]
async fn routines_and_run_chats_survive_restart_and_delete() {
    let fixture = Fixture::new();
    let provider = provider();
    let engine = setup(&fixture, &provider).await;
    let routine = create(&engine, json!({})).await;
    let id = routine["id"].as_str().unwrap().to_string();
    let chat_id = run_now(&engine, &id).await;
    wait_for_requests(&provider, 1).await;
    drop(engine);

    let engine = fixture.engine_with_clock(&provider, clock());
    let listed = list(&engine).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["runs"][0]["chatId"], chat_id);

    engine
        .handle(methods::DELETE_ROUTINE, json!({ "routineId": id }))
        .await
        .unwrap();
    let row = chat_row(&engine, &chat_id)
        .await
        .expect("run chats outlive their Routine");
    assert_eq!(row["routineRun"]["routineName"], "Daily digest");
    let error = engine
        .handle(methods::RUN_ROUTINE_NOW, json!({ "routineId": id }))
        .await
        .err()
        .expect("a deleted Routine cannot run");
    assert!(matches!(error, RpcError::BadParams(_)));
}

fn init_repo(dir: &Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let sig = git2::Signature::now("t", "t@t").unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

#[tokio::test]
async fn new_worktree_routines_run_in_a_session_worktree() {
    let fixture = Fixture::new();
    init_repo(fixture.project_dir.path());
    let provider = provider();
    let engine = setup(&fixture, &provider).await;
    let routine = create(&engine, json!({ "checkout": "new-worktree" })).await;

    let chat_id = run_now(&engine, routine["id"].as_str().unwrap()).await;
    let row = chat_row(&engine, &chat_id).await.unwrap();
    assert_eq!(
        row["worktree"],
        json!({ "repoPath": fixture.cwd(), "base": "HEAD" })
    );
    wait_for_requests(&provider, 1).await;
    assert!(
        fixture
            .data_dir
            .path()
            .join("worktrees")
            .join(&chat_id)
            .exists()
    );
}
