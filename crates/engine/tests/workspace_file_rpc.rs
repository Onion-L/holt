//! `ReadWorkspaceFile` over the handle seam: the root fence plus its one
//! exception — a skill's `SKILL.md` reads through its absolute location in
//! the personal/holt skill roots (the sidebar's file tab opens it from the
//! transcript's skill chips), while saves and non-skill paths stay
//! workspace-fenced.

mod common;

use common::{Fixture, ScriptedProvider};
use holt_engine::LocalEngine;
use holt_proto::WorkspaceFileRead;
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::json;

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

async fn read(engine: &LocalEngine, path: &str) -> Result<WorkspaceFileRead, RpcError> {
    match engine
        .handle(
            methods::READ_WORKSPACE_FILE,
            json!({ "spaceId": "space-1", "path": path }),
        )
        .await?
    {
        RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
        _ => panic!("ReadWorkspaceFile must reply with a value"),
    }
}

#[tokio::test]
async fn a_personal_skill_file_reads_through_its_absolute_path() {
    let fixture = Fixture::new();
    let engine = setup(&fixture).await;

    let dir = fixture.personal_dir.path().join("research");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# research\n").unwrap();
    let skill_path = dir.join("SKILL.md");

    let read = read(&engine, &skill_path.display().to_string())
        .await
        .expect("the personal skill root is readable");
    assert_eq!(read.text.as_deref(), Some("# research\n"));
}

#[tokio::test]
async fn non_skill_paths_outside_the_root_stay_refused() {
    let fixture = Fixture::new();
    let engine = setup(&fixture).await;

    let outside = fixture.personal_dir.path().join("plain.txt");
    std::fs::write(&outside, b"x").unwrap();

    let error = match read(&engine, &outside.display().to_string()).await {
        Ok(_) => panic!("a non-skill outside-root file is still fenced"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("outside"), "{error}");
}

#[tokio::test]
async fn saves_stay_workspace_fenced_even_for_skill_roots() {
    let fixture = Fixture::new();
    let engine = setup(&fixture).await;

    let dir = fixture.personal_dir.path().join("research");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"# research\n").unwrap();
    let skill_path = dir.join("SKILL.md");

    let error = match engine
        .handle(
            methods::SAVE_WORKSPACE_FILE,
            json!({
                "spaceId": "space-1",
                "path": skill_path.display().to_string(),
                "text": "edited",
                "version": "",
                "bom": false,
            }),
        )
        .await
    {
        Ok(_) => panic!("a save outside the root refuses even in a skill root"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("outside"), "{error}");
}
