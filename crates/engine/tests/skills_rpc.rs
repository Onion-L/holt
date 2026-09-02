//! Handle-seam tests for the skills capability (ADR-0005): a real engine
//! assembled on a temp data dir with temp directories standing in for the
//! three skill roots — project (via the `cwd` param), personal (pinned
//! through `EngineConfig`), holt (`<data_dir>/skills`) — driven through the
//! `RpcService` trait exactly as the UI drives it. The scans are fresh per
//! call, so entries can appear and disappear mid-test without rebuilds.

use holt_engine::{EngineConfig, StubEngine};
use holt_proto::{SkillListing, SkillRoot};
use holt_rpc::{RpcReply, RpcService, methods};
use tempfile::TempDir;

struct Fixture {
    /// Stands in for the chat's working directory; its
    /// `.agents/skills` is the project root.
    project_dir: TempDir,
    /// The personal skill root override.
    personal_dir: TempDir,
    /// The engine's own data dir; `skills/` under it is the holt root.
    data_dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            project_dir: TempDir::new().unwrap(),
            personal_dir: TempDir::new().unwrap(),
            data_dir: TempDir::new().unwrap(),
        }
    }

    fn engine(&self) -> StubEngine {
        StubEngine::assemble(&EngineConfig {
            data_dir: self.data_dir.path().to_path_buf(),
            personal_skills_dir: Some(self.personal_dir.path().to_path_buf()),
        })
        .unwrap()
    }

    fn cwd(&self) -> String {
        self.project_dir.path().display().to_string()
    }

    async fn list(&self, engine: &StubEngine) -> SkillListing {
        let RpcReply::Value(value) = engine
            .handle(
                methods::LIST_SKILLS,
                serde_json::json!({ "cwd": self.cwd() }),
            )
            .await
            .unwrap()
        else {
            panic!("ListSkills did not return a value");
        };
        serde_json::from_value(value).unwrap()
    }
}

/// Write a valid skill directory (`<name>/SKILL.md`) under `root` and
/// return the SKILL.md path.
fn skill(root: &std::path::Path, name: &str, description: &str) -> String {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    std::fs::write(
        &file,
        format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
    )
    .unwrap();
    file.display().to_string()
}

/// Write a `SKILL.md` with arbitrary frontmatter under `<root>/<dir-name>`.
fn raw_skill(root: &std::path::Path, dir_name: &str, frontmatter: &str) -> String {
    let dir = root.join(dir_name);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    std::fs::write(&file, format!("---\n{frontmatter}---\nbody\n")).unwrap();
    file.display().to_string()
}

fn project_root(fixture: &Fixture) -> std::path::PathBuf {
    fixture.project_dir.path().join(".agents").join("skills")
}

fn holt_root(fixture: &Fixture) -> std::path::PathBuf {
    fixture.data_dir.path().join("skills")
}

#[tokio::test]
async fn list_skills_discovers_all_three_roots_fresh_per_call() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;

    // Fresh machine: no root exists, the catalog is empty, not an error.
    assert_eq!(fixture.list(engine).await, SkillListing::default());

    let project_file = skill(&project_root(&fixture), "project-tool", "From the project.");
    let personal_file = skill(
        fixture.personal_dir.path(),
        "personal-tool",
        "From personal.",
    );
    let holt_file = skill(&holt_root(&fixture), "holt-tool", "From holt.");

    let listing = fixture.list(engine).await;
    let by_name: Vec<(String, SkillRoot)> = listing
        .skills
        .iter()
        .map(|entry| (entry.name.clone(), entry.root))
        .collect();
    assert_eq!(
        by_name,
        vec![
            ("project-tool".into(), SkillRoot::Project),
            ("personal-tool".into(), SkillRoot::Personal),
            ("holt-tool".into(), SkillRoot::Holt),
        ]
    );
    let files: Vec<&str> = listing.skills.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(files, vec![project_file, personal_file, holt_file]);
    assert!(
        listing
            .skills
            .iter()
            .all(|entry| !entry.disable_model_invocation)
    );

    // A skill dropped in after the first call is picked up by the next one —
    // the filesystem is the registry, there is no cache.
    skill(fixture.personal_dir.path(), "late-arrival", "Added later.");
    let listing = fixture.list(engine).await;
    assert!(
        listing
            .skills
            .iter()
            .any(|entry| entry.name == "late-arrival")
    );
}

#[tokio::test]
async fn nearest_root_wins_and_losers_name_the_winner() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;

    // personal beats holt.
    skill(
        fixture.personal_dir.path(),
        "shared",
        "Personal definition.",
    );
    skill(&holt_root(&fixture), "shared", "Holt definition.");
    let listing = fixture.list(engine).await;
    assert_eq!(listing.skills.len(), 1);
    assert_eq!(listing.skills[0].root, SkillRoot::Personal);
    assert_eq!(listing.shadowed.len(), 1);
    assert_eq!(listing.shadowed[0].root, SkillRoot::Holt);
    assert_eq!(listing.shadowed[0].shadowed_by, SkillRoot::Personal);

    // …and project beats both.
    let project_file = skill(&project_root(&fixture), "shared", "Project definition.");
    let listing = fixture.list(engine).await;
    assert_eq!(listing.skills.len(), 1);
    assert_eq!(listing.skills[0].file, project_file);
    let losers: Vec<SkillRoot> = listing.shadowed.iter().map(|e| e.root).collect();
    assert_eq!(losers, vec![SkillRoot::Personal, SkillRoot::Holt]);
    assert!(
        listing
            .shadowed
            .iter()
            .all(|entry| entry.shadowed_by == SkillRoot::Project)
    );
}

#[tokio::test]
async fn invalid_skills_carry_loader_diagnostics_and_stay_uninvocable() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;

    let mismatched = raw_skill(
        fixture.personal_dir.path(),
        "actual-dir",
        "name: wrong-name\ndescription: fine.\n",
    );
    let undescribed = raw_skill(
        fixture.personal_dir.path(),
        "no-description",
        "name: no-description\n",
    );

    let listing = fixture.list(engine).await;
    assert_eq!(listing.skills, vec![]);
    assert_eq!(listing.shadowed, vec![]);
    assert_eq!(listing.invalid.len(), 2);
    let mismatch = listing
        .invalid
        .iter()
        .find(|entry| entry.file == mismatched)
        .expect("mismatched-name entry missing");
    assert_eq!(mismatch.name.as_deref(), Some("wrong-name"));
    assert_eq!(mismatch.root, SkillRoot::Personal);
    assert!(mismatch.message.contains("does not match parent directory"));
    let undescribed = listing
        .invalid
        .iter()
        .find(|entry| entry.file == undescribed)
        .expect("missing-description entry missing");
    assert!(undescribed.message.contains("description is required"));
}

#[tokio::test]
async fn ignore_files_keep_entries_out_of_the_catalog() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;

    let root = fixture.personal_dir.path();
    skill(root, "kept", "Kept.");
    skill(root, "drafts", "A draft.");
    std::fs::write(root.join(".gitignore"), "drafts/\n").unwrap();

    let listing = fixture.list(engine).await;
    assert_eq!(
        listing
            .skills
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(listing.invalid, vec![]);
}

/// Queue an `invokeSkill` command exactly as the composer serializes it.
async fn invoke(
    engine: &StubEngine,
    cwd: &str,
    name: &str,
    extra: Option<&str>,
) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
    let command = serde_json::json!({
        "kind": "invokeSkill",
        "name": name,
        "extraInstructions": extra,
        "messageId": format!("message-{name}"),
        "request": {
            "prompt": "",
            "provider": "openai",
            "model": "openai/gpt-5.4",
            "reasoning": null,
            "modelOptions": {},
            "cwd": cwd,
            "sandbox": "workspace-write"
        }
    });
    engine
        .handle(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": "chat-1", "command": command }),
        )
        .await
}

#[tokio::test]
async fn invoking_a_skill_chips_in_the_transcript_and_unknown_names_fail() {
    use futures::StreamExt as _;
    use holt_doc::{MessagePart, MessageRole, TranscriptFrame};

    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;
    let skill_file = skill(fixture.personal_dir.path(), "grill", "Grill a plan.");

    // The run acceptance path needs a configured provider; the run itself
    // dies on its own against the fake key — only pre-run behavior is
    // asserted.
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();
    let RpcReply::Stream(mut transcript) = engine
        .handle(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": "chat-1" }),
        )
        .await
        .unwrap()
    else {
        panic!("WatchDocMessages did not return a stream");
    };
    assert_eq!(
        transcript.next().await.unwrap(),
        serde_json::json!({ "reset": [] })
    );

    // Unknown skill: immediate error reply, no transcript entry, no run.
    let error = match invoke(engine, &fixture.cwd(), "nope", None).await {
        Err(error) => error,
        Ok(_) => panic!("unknown skill was accepted"),
    };
    assert!(error.to_string().contains("unknown skill"), "{error}");

    // Accepted invocation: a user entry whose first part is the skill chip
    // (name + source pointer) followed by the extra instructions verbatim.
    invoke(
        engine,
        &fixture.cwd(),
        "grill",
        Some("focus on the data layer"),
    )
    .await
    .unwrap();
    let frame: TranscriptFrame = serde_json::from_value(transcript.next().await.unwrap()).unwrap();
    let holt_doc::TranscriptFrame::Delta { upsert, .. } = frame else {
        panic!("expected a delta frame");
    };
    assert_eq!(upsert.len(), 1);
    let entry = &upsert[0].entry;
    assert_eq!(entry.role, MessageRole::User);
    assert_eq!(entry.parts.len(), 2);
    match &entry.parts[0] {
        MessagePart::Skill { name, file, .. } => {
            assert_eq!(name, "grill");
            assert_eq!(file, skill_file.as_str());
        }
        other => panic!("expected a skill chip, got {other:?}"),
    }
    match &entry.parts[1] {
        MessagePart::Text { text, .. } => assert_eq!(text, "focus on the data layer"),
        other => panic!("expected the extra text, got {other:?}"),
    }
    // The transcript carries the chip and the user's own words — never the
    // skill content, description, or the raw `/skill` directive.
    let serialized = serde_json::to_string(entry).unwrap();
    assert!(!serialized.contains("Grill a plan."), "{serialized}");
    assert!(!serialized.contains("# grill"), "{serialized}");
    assert!(!serialized.contains("/skill"), "{serialized}");
}

#[tokio::test]
async fn an_invocation_while_a_run_is_active_is_rejected_like_a_second_run() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let engine = &engine;
    skill(fixture.personal_dir.path(), "grill", "Grill a plan.");
    engine
        .handle(
            methods::SAVE_PROVIDER_KEY,
            serde_json::json!({ "providerId": "openai", "key": "not-a-real-key" }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": "chat-1" }),
        )
        .await
        .unwrap();

    // Accepted: the chat is now running.
    invoke(engine, &fixture.cwd(), "grill", None).await.unwrap();
    // While it runs, a second invocation rides the same acceptance rules
    // as a second ordinary Run (spec story 23: consistent with everything
    // else the user sends) — no queue, a clear error.
    let error = match invoke(engine, &fixture.cwd(), "grill", None).await {
        Err(error) => error,
        Ok(_) => panic!("invocation during an active run was accepted"),
    };
    assert!(error.to_string().contains("already running"), "{error}");
}
