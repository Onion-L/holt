//! Port of `prompt-templates.test.ts`, `system-prompt.test.ts`,
//! `skills.test.ts`, `resource-formatting.test.ts`, and
//! `session/search.test.ts`.

mod common;

use std::sync::Arc;

use serde_json::json;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::prompt_templates::{
    format_prompt_template_invocation, load_prompt_templates, parse_command_args, substitute_args,
};
use pi_core::agent::harness::session::memory::{InMemorySessionStorage, Session};
use pi_core::agent::harness::session::types::SessionMetadata;
use pi_core::agent::harness::skills::{format_skill_invocation, load_skills, load_sourced_skills};
use pi_core::agent::harness::system_prompt::format_skills_for_system_prompt;
use pi_core::agent::harness::types::{ExecutionEnv, FileSystem, Skill, WriteContent};
use pi_core::agent::search::{
    ScanningReadableOptions, ScanningSessionSearchOptions, SessionSearchOptions, StorageReadable,
    create_scanning_session_search,
};
use pi_core::agent::types::AgentMessage;
use pi_core::ai::types::{BlockContent, RoleUser, TextContent, UserContent, UserMessage};

fn env_for(root: &str) -> Arc<NodeExecutionEnv> {
    Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }))
}

fn env_trait(env: &Arc<NodeExecutionEnv>) -> Arc<dyn ExecutionEnv> {
    Arc::clone(env) as Arc<dyn ExecutionEnv>
}

async fn write(env: &Arc<NodeExecutionEnv>, path: &str, content: &str) {
    env.write_file(path, &WriteContent::Text(content.to_string()), None)
        .await
        .expect("write");
}

// ---------------------------------------------------------------------------
// prompt-templates.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loads_templates_from_directories_and_files() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "templates",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "templates/review.md",
        "---\ndescription: Review code\n---\nReview $1 with care",
    )
    .await;
    write(&env, "templates/notes.txt", "not markdown").await;
    write(&env, "standalone.md", "Standalone content").await;

    let loaded = load_prompt_templates(
        &env_trait(&env),
        &[
            "templates".to_string(),
            "standalone.md".to_string(),
            "missing".to_string(),
        ],
    )
    .await;

    assert!(loaded.diagnostics.is_empty());
    assert_eq!(loaded.prompt_templates.len(), 2);
    assert_eq!(loaded.prompt_templates[0].name, "review");
    assert_eq!(
        loaded.prompt_templates[0].description.as_deref(),
        Some("Review code")
    );
    assert_eq!(loaded.prompt_templates[1].name, "standalone");
    // First body line becomes the description when frontmatter omits one.
    assert_eq!(
        loaded.prompt_templates[1].description.as_deref(),
        Some("Standalone content")
    );
}

#[tokio::test]
async fn parse_command_args_handles_quotes() {
    assert_eq!(
        parse_command_args("a \"b c\" 'd e'"),
        ["a".to_string(), "b c".to_string(), "d e".to_string()]
    );
    assert_eq!(
        parse_command_args("  spaced   out  "),
        ["spaced".to_string(), "out".to_string()]
    );
}

#[test]
fn substitute_args_replaces_all_placeholder_forms() {
    let args: Vec<String> = ["one".to_string(), "two".to_string(), "three".to_string()].to_vec();
    assert_eq!(substitute_args("$1 $2 $3", &args), "one two three");
    assert_eq!(substitute_args("$5 missing", &args), " missing");
    assert_eq!(substitute_args("${@:2}", &args), "two three");
    assert_eq!(substitute_args("${@:1:2}", &args), "one two");
    assert_eq!(substitute_args("$@", &args), "one two three");
    assert_eq!(substitute_args("$ARGUMENTS", &args), "one two three");
}

#[test]
fn format_prompt_template_invocation_substitutes() {
    let template = pi_core::agent::harness::types::PromptTemplate {
        name: "review".to_string(),
        description: None,
        content: "Review $1 with $ARGUMENTS".to_string(),
    };
    assert_eq!(
        format_prompt_template_invocation(&template, &["a.ts".to_string(), "care".to_string()]),
        "Review a.ts with a.ts care"
    );
}

// ---------------------------------------------------------------------------
// system-prompt.test.ts + resource-formatting.test.ts
// ---------------------------------------------------------------------------

fn skill(name: &str, description: &str, path: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        content: format!("Use {name}."),
        file_path: path.to_string(),
        disable_model_invocation: None,
    }
}

#[test]
fn formats_skills_for_system_prompt() {
    let formatted = format_skills_for_system_prompt(&[
        skill("a", "A skill", "/skills/a/SKILL.md"),
        skill("b<>&'\"", "B skill", "/skills/b/SKILL.md"),
    ]);
    assert!(formatted.starts_with(
        "The following skills provide specialized instructions for specific tasks.\nRead the full skill file when the task matches its description.\n"
    ));
    assert!(formatted.contains("<available_skills>"));
    assert!(formatted.contains("<name>a</name>"));
    assert!(formatted.contains("<description>B skill</description>"));
    assert!(formatted.contains("<name>b&lt;&gt;&amp;&apos;&quot;</name>"));
    assert!(formatted.contains("<location>/skills/a/SKILL.md</location>"));
    assert!(formatted.ends_with("</available_skills>"));
}

#[test]
fn hides_disabled_skills_and_empty_lists() {
    let mut disabled = skill("hidden", "Hidden", "/skills/hidden/SKILL.md");
    disabled.disable_model_invocation = Some(true);
    assert_eq!(format_skills_for_system_prompt(&[]), "");
    assert_eq!(format_skills_for_system_prompt(&[disabled]), "");
}

#[test]
fn formats_skill_invocations_with_additional_instructions() {
    let inspect = Skill {
        name: "inspect".to_string(),
        description: "Inspect things".to_string(),
        content: "Use inspection tools.".to_string(),
        file_path: "/project/.pi/skills/inspect/SKILL.md".to_string(),
        disable_model_invocation: None,
    };

    assert_eq!(
        format_skill_invocation(&inspect, Some("Check errors.")),
        "<skill name=\"inspect\" location=\"/project/.pi/skills/inspect/SKILL.md\">\nReferences are relative to /project/.pi/skills/inspect.\n\nUse inspection tools.\n</skill>\n\nCheck errors."
    );
    assert_eq!(
        format_skill_invocation(&inspect, None),
        "<skill name=\"inspect\" location=\"/project/.pi/skills/inspect/SKILL.md\">\nReferences are relative to /project/.pi/skills/inspect.\n\nUse inspection tools.\n</skill>"
    );
}

// ---------------------------------------------------------------------------
// skills.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loads_skill_md_files_through_the_execution_environment() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        ".agents/skills/example",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        ".agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nUse this skill.",
    )
    .await;

    let loaded = load_skills(&env_trait(&env), &[".agents/skills".to_string()]).await;

    assert!(loaded.diagnostics.is_empty());
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].name, "example");
    assert_eq!(loaded.skills[0].description, "Example skill");
    assert_eq!(loaded.skills[0].content, "Use this skill.");
    assert_eq!(
        loaded.skills[0].file_path,
        format!("{root}/.agents/skills/example/SKILL.md")
    );
    assert_eq!(loaded.skills[0].disable_model_invocation, Some(true));
}

#[tokio::test]
async fn loads_skills_through_symlinked_directories() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "actual/example",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "actual/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\n---\nUse this skill.",
    )
    .await;
    std::os::unix::fs::symlink(format!("{root}/actual"), format!("{root}/skills-link")).unwrap();

    let loaded = load_skills(&env_trait(&env), &["skills-link".to_string()]).await;

    assert_eq!(
        loaded
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["example"]
    );
    assert_eq!(
        loaded.skills[0].file_path,
        format!("{root}/skills-link/example/SKILL.md")
    );
}

#[tokio::test]
async fn preserves_source_info_for_sourced_skills() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "user/example",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "user/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\n---\nUse this skill.",
    )
    .await;

    let (skills, diagnostics) = load_sourced_skills(
        &env_trait(&env),
        &[("user".to_string(), json!({ "type": "user" }))],
    )
    .await;

    assert!(diagnostics.is_empty());
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].0.name, "example");
    assert_eq!(skills[0].0.description, "Example skill");
    assert_eq!(skills[0].0.content, "Use this skill.");
    assert_eq!(
        skills[0].0.file_path,
        format!("{root}/user/example/SKILL.md")
    );
    assert_eq!(skills[0].0.disable_model_invocation, Some(false));
    assert_eq!(skills[0].1, json!({ "type": "user" }));
}

#[tokio::test]
async fn attaches_source_info_to_diagnostics() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "user/broken",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "user/broken/SKILL.md",
        "---\nname: broken\n---\nMissing description.",
    )
    .await;

    let (skills, diagnostics) = load_sourced_skills(
        &env_trait(&env),
        &[("user".to_string(), json!({ "type": "user" }))],
    )
    .await;

    assert!(skills.is_empty());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].0.message, "description is required",);
    assert_eq!(
        diagnostics[0].0.path,
        format!("{root}/user/broken/SKILL.md")
    );
    assert_eq!(diagnostics[0].1, json!({ "type": "user" }));
}

#[tokio::test]
async fn loads_direct_markdown_children_only_from_the_root_directory() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "skills/nested",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "skills/root.md",
        "---\ndescription: Root skill\n---\nRoot content",
    )
    .await;
    write(
        &env,
        "skills/nested/ignored.md",
        "---\ndescription: Ignored\n---\nIgnored content",
    )
    .await;

    let loaded = load_skills(&env_trait(&env), &["skills".to_string()]).await;

    assert_eq!(
        loaded
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["skills"]
    );
    assert_eq!(loaded.skills[0].content, "Root content");
}

#[tokio::test]
async fn ignores_root_markdown_docs_that_do_not_declare_skills() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "skills/nested-skill",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "skills/README.md",
        "# Shared skills\n\nDocumentation.",
    )
    .await;
    write(&env, "skills/AGENTS.md", "# Agent notes\n\nDocumentation.").await;
    write(
        &env,
        "skills/CLAUDE.md",
        "---\ndescription: [invalid\n---\n\nDocumentation.",
    )
    .await;
    write(
        &env,
        "skills/root.md",
        "---\ndescription: Root skill\n---\nRoot content",
    )
    .await;
    write(
        &env,
        "skills/nested-skill/SKILL.md",
        "---\nname: nested-skill\ndescription: Nested skill\n---\nNested content",
    )
    .await;

    let loaded = load_skills(&env_trait(&env), &["skills".to_string()]).await;

    assert!(loaded.diagnostics.is_empty());
    let mut names: Vec<&str> = loaded
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["nested-skill", "skills"]);
}

// ---------------------------------------------------------------------------
// session/search.test.ts
// ---------------------------------------------------------------------------

fn message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Blocks(vec![BlockContent::Text(TextContent {
            text: text.to_string(),
            ..Default::default()
        })]),
        timestamp: 1,
    })
}

fn memory_session(id: &str) -> Arc<InMemorySessionStorage> {
    Arc::new(InMemorySessionStorage::new(SessionMetadata {
        id: id.to_string(),
        created_at: 1,
        parent_session_id: None,
    }))
}

fn project_text(
    _metadata: &pi_core::agent::harness::session::jsonl::types::JsonlSessionMetadata,
    entry: &pi_core::agent::harness::session::types::Entry,
    _label: Option<&str>,
) -> String {
    match entry {
        pi_core::agent::harness::session::types::Entry::Message {
            message: AgentMessage::User(user),
            ..
        } => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    BlockContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        _ => String::new(),
    }
}

#[tokio::test]
async fn scans_an_arbitrary_in_memory_projected_source() {
    let root = memory_session("root");
    let session = Session::new(
        Arc::clone(&root) as Arc<dyn pi_core::agent::harness::session::types::SessionStorage>
    );
    session
        .append_message(message("fix auth flow"))
        .await
        .unwrap();
    session
        .append_message(message("unrelated chatter"))
        .await
        .unwrap();

    let readable: Arc<dyn pi_core::agent::search::ScanningReadable> =
        Arc::new(StorageReadable(Arc::clone(&root)
            as Arc<
                dyn pi_core::agent::harness::session::types::SessionStorage,
            >));
    let readables = vec![readable];
    let source: pi_core::agent::search::ScanningReadableSource =
        Arc::new(move |_options| Box::pin(futures::future::ready(readables.clone())));
    let search = create_scanning_session_search(
        source,
        ScanningSessionSearchOptions {
            base: ScanningReadableOptions {
                project_text: Some(Arc::new(project_text)),
                page_size: None,
            },
            source_options: None,
            matcher: None,
            create_hit: None,
        },
    );

    let hits = search
        .search("auth", Some(SessionSearchOptions::default()))
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "root");
    assert!(hits[0].snippet.contains("fix auth flow"));

    let hits = search
        .search("missing", Some(SessionSearchOptions::default()))
        .await
        .expect("search");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn applies_limit_and_entry_type_filters() {
    let root = memory_session("root");
    let session = Session::new(
        Arc::clone(&root) as Arc<dyn pi_core::agent::harness::session::types::SessionStorage>
    );
    session
        .append_message(message("fix auth flow"))
        .await
        .unwrap();
    session.append_message(message("auth again")).await.unwrap();
    session.append_message(message("auth third")).await.unwrap();

    let readable: Arc<dyn pi_core::agent::search::ScanningReadable> =
        Arc::new(StorageReadable(Arc::clone(&root)
            as Arc<
                dyn pi_core::agent::harness::session::types::SessionStorage,
            >));
    let readables = vec![readable];
    let source: pi_core::agent::search::ScanningReadableSource =
        Arc::new(move |_options| Box::pin(futures::future::ready(readables.clone())));
    let search = create_scanning_session_search(
        source,
        ScanningSessionSearchOptions {
            base: ScanningReadableOptions {
                project_text: Some(Arc::new(project_text)),
                page_size: None,
            },
            source_options: None,
            matcher: None,
            create_hit: None,
        },
    );

    let hits = search
        .search(
            "auth",
            Some(SessionSearchOptions {
                limit: Some(2),
                ..Default::default()
            }),
        )
        .await
        .expect("search");
    assert_eq!(hits.len(), 2);

    let hits = search
        .search(
            "auth",
            Some(SessionSearchOptions {
                entry_types: Some(Vec::new()),
                ..Default::default()
            }),
        )
        .await
        .expect("search");
    assert!(hits.is_empty());

    let hits = search
        .search("  ", Some(SessionSearchOptions::default()))
        .await
        .expect("search");
    assert!(hits.is_empty());
}

// ---------------------------------------------------------------------------
// prompt-templates.test.ts "preserves source info for sourced prompt
// templates" and the search.test.ts label/filter/JSONL cases.

/// Port of "preserves source info for sourced prompt templates".
#[tokio::test]
async fn preserves_source_info_for_sourced_prompt_templates() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir(
        "prompts",
        pi_core::agent::harness::types::CreateDirOptions::default(),
        None,
    )
    .await
    .unwrap();
    write(
        &env,
        "prompts/example.md",
        "---\ndescription: Example\n---\nExample body",
    )
    .await;

    let (templates, diagnostics) =
        pi_core::agent::harness::prompt_templates::load_sourced_prompt_templates(
            &env_trait(&env),
            &[pi_core::agent::harness::prompt_templates::SourcedInput {
                path: "prompts".to_string(),
                source: json!({ "type": "project" }),
            }],
        )
        .await;

    assert!(diagnostics.is_empty());
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].prompt_template.name, "example");
    assert_eq!(
        templates[0].prompt_template.description.as_deref(),
        Some("Example")
    );
    assert_eq!(templates[0].prompt_template.content, "Example body");
    assert_eq!(templates[0].source, json!({ "type": "project" }));
}

/// The sourced prompt-template diagnostics carry the source too (the TS
/// `attaches source info to diagnostics` case for templates).
#[tokio::test]
async fn attaches_source_info_to_prompt_template_diagnostics() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    write(
        &env,
        "broken.md",
        "---\ndescription: [unterminated\n---\nBody",
    )
    .await;

    let (templates, diagnostics) =
        pi_core::agent::harness::prompt_templates::load_sourced_prompt_templates(
            &env_trait(&env),
            &[pi_core::agent::harness::prompt_templates::SourcedInput {
                path: "broken.md".to_string(),
                source: json!({ "type": "user" }),
            }],
        )
        .await;

    assert!(templates.is_empty());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].diagnostic.path, format!("{root}/broken.md"));
    assert_eq!(diagnostics[0].source, json!({ "type": "user" }));
}

// ---------------------------------------------------------------------------
// search.test.ts

fn search_message(text: &str) -> pi_core::agent::types::AgentMessage {
    pi_core::agent::types::AgentMessage::User(pi_core::ai::types::UserMessage {
        role: pi_core::ai::types::RoleUser,
        content: pi_core::ai::types::UserContent::Blocks(vec![
            pi_core::ai::types::BlockContent::Text(pi_core::ai::types::TextContent {
                text: text.to_string(),
                ..Default::default()
            }),
        ]),
        timestamp: 1,
    })
}

fn deterministic_memory_session(
    id: &str,
    created_at: i64,
) -> pi_core::agent::harness::session::memory::Session {
    let storage = pi_core::agent::harness::session::memory::InMemorySessionStorage::new(
        pi_core::agent::harness::session::types::SessionMetadata {
            id: id.to_string(),
            created_at,
            parent_session_id: None,
        },
    );
    let next_id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    pi_core::agent::harness::session::memory::Session::with_id_generator(
        std::sync::Arc::new(storage),
        std::sync::Arc::new(move || {
            format!(
                "entry-{}",
                next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            )
        }),
    )
}

async fn collect_hits(
    search: &std::sync::Arc<dyn pi_core::agent::search::SessionSearch>,
    text: &str,
    options: Option<pi_core::agent::search::SessionSearchOptions>,
) -> Vec<pi_core::agent::search::ScanningSessionSearchHit> {
    search.search(text, options).await.expect("search")
}

fn scanning_source(
    storages: Vec<Arc<dyn pi_core::agent::harness::session::types::SessionStorage>>,
) -> pi_core::agent::search::ScanningReadableSource {
    let readables: Vec<Arc<dyn pi_core::agent::search::ScanningReadable>> = storages
        .into_iter()
        .map(|storage| {
            Arc::new(pi_core::agent::search::StorageReadable(storage))
                as Arc<dyn pi_core::agent::search::ScanningReadable>
        })
        .collect();
    Arc::new(move |_options| Box::pin(futures::future::ready(readables.clone())))
}

/// Port of "includes labels in memory scanning projections".
#[tokio::test]
async fn includes_labels_in_memory_scanning_projections() {
    let session = deterministic_memory_session("session", 1);
    let entry_id = session
        .append_message(search_message("plain body"))
        .await
        .expect("append");
    session
        .set_label(&entry_id, Some("important label".to_string()))
        .await
        .expect("set label");
    let search = pi_core::agent::search::create_scanning_session_search(
        scanning_source(vec![Arc::clone(session.storage())]),
        pi_core::agent::search::ScanningSessionSearchOptions::default(),
    );

    let hits = collect_hits(&search, "important", None).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "session");
    assert_eq!(hits[0].entry_id, entry_id);
}

/// Port of "honors entry type filters and abort signals in scanning search".
#[tokio::test]
async fn honors_entry_type_filters_and_abort_signals_in_scanning_search() {
    let session = deterministic_memory_session("session", 1);
    let message_entry_id = session
        .append_message(search_message("auth message"))
        .await
        .expect("append message");
    session
        .append_custom_entry("note", Some(json!({ "text": "auth custom" })))
        .await
        .expect("append custom");
    let search = pi_core::agent::search::create_scanning_session_search(
        scanning_source(vec![Arc::clone(session.storage())]),
        pi_core::agent::search::ScanningSessionSearchOptions::default(),
    );

    let hits = collect_hits(
        &search,
        "auth",
        Some(pi_core::agent::search::SessionSearchOptions {
            entry_types: Some(vec![
                pi_core::agent::harness::session::types::EntryType::Message,
            ]),
            limit: None,
            signal: None,
        }),
    )
    .await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "session");
    assert_eq!(hits[0].entry_id, message_entry_id);

    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let aborted = search
        .search(
            "auth",
            Some(pi_core::agent::search::SessionSearchOptions {
                entry_types: None,
                limit: None,
                signal: Some(signal),
            }),
        )
        .await
        .expect_err("aborted search");
    assert!(
        aborted.contains("aborted") || aborted.contains("Abort"),
        "expected an abort error, got {aborted}"
    );
}

/// Port of "scans JSONL sessions from disk through the JSONL scanning
/// source".
#[tokio::test]
async fn scans_jsonl_sessions_from_disk_through_the_jsonl_source() {
    let root = common::create_temp_dir();
    let env: Arc<NodeExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }));
    let options = pi_core::agent::harness::session::jsonl::types::JsonlSessionRepoOptions {
        fs: env.clone(),
        sessions_root: root.to_string(),
    };
    let repository =
        Arc::new(pi_core::agent::harness::session::jsonl::repo::JsonlSessionRepo::new(options));
    let session = repository
        .create(
            pi_core::agent::harness::session::jsonl::types::JsonlSessionCreateOptions {
                id: Some("jsonl".to_string()),
                cwd: format!("{root}/workspace"),
                ..Default::default()
            },
        )
        .await
        .expect("create jsonl session");
    let entry_id = session
        .append_message(search_message("jsonl backed auth entry"))
        .await
        .expect("append");
    session
        .set_label(&entry_id, Some("disk label".to_string()))
        .await
        .expect("set label");
    let other = repository
        .create(
            pi_core::agent::harness::session::jsonl::types::JsonlSessionCreateOptions {
                id: Some("other".to_string()),
                cwd: format!("{root}/other"),
                ..Default::default()
            },
        )
        .await
        .expect("create other session");
    let other_entry_id = other
        .append_message(search_message("jsonl backed auth entry in another cwd"))
        .await
        .expect("append other");

    let source_repository = Arc::clone(&repository);
    let source: pi_core::agent::search::ScanningReadableSource = Arc::new(move |_options| {
        let repository = Arc::clone(&source_repository);
        Box::pin(async move {
            let metadata = repository
                .list(
                    &pi_core::agent::harness::session::jsonl::types::JsonlSessionListOptions::default(),
                )
                .await
                .expect("list JSONL sessions");
            let mut readables: Vec<Arc<dyn pi_core::agent::search::ScanningReadable>> = Vec::new();
            for metadata in metadata {
                let session = repository.open(metadata).await.expect("open JSONL session");
                readables.push(Arc::new(pi_core::agent::search::StorageReadable(
                    Arc::clone(session.storage()),
                )));
            }
            readables
        })
    });
    let search = pi_core::agent::search::create_scanning_session_search(
        source,
        pi_core::agent::search::ScanningSessionSearchOptions::default(),
    );

    let auth_hits = collect_hits(&search, "auth", None).await;
    assert_eq!(auth_hits.len(), 2);
    let mut seen = Vec::new();
    for hit in &auth_hits {
        seen.push((hit.session_id.as_str(), hit.entry_id.as_str()));
    }
    assert!(seen.contains(&("jsonl", entry_id.as_str())));
    assert!(seen.contains(&("other", other_entry_id.as_str())));

    let label_hits = collect_hits(&search, "disk", None).await;
    assert_eq!(label_hits.len(), 1);
    assert_eq!(label_hits[0].session_id, "jsonl");
    assert_eq!(label_hits[0].entry_id, entry_id);
}
