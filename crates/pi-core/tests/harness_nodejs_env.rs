//! Port of `pi-core/agent/test/harness/nodejs-env.test.ts`.
//!
//! Deviations from the TypeScript suite, all platform-bound:
//! - "uses stdin command transport for legacy WSL bash paths" fakes
//!   `process.platform = "win32"`, which cannot be reproduced from Rust;
//!   the stdin transport code path itself is ported.
//! - The two `skipIf(process.platform !== "win32")` cases (detached
//!   descendant stdio grace, taskkill spawn errors) skip identically.
//! - `mtimeMs` is a float in both suites.

mod common;

use std::sync::Arc;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::types::{
    CreateDirOptions, CreateTempFileOptions, FileKind, FileSystem, ReadTextLinesOptions,
    RemoveOptions, Shell, ShellExecOptions, WriteContent,
};
use pi_core::agent::harness::utils::shell_output::execute_shell_with_capture;

fn env_for(cwd: &str) -> NodeExecutionEnv {
    NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: cwd.to_string(),
        ..Default::default()
    })
}

fn shell_env_for(cwd: &str, shell_env: Vec<(&str, &str)>) -> NodeExecutionEnv {
    NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: cwd.to_string(),
        shell_env: Some(
            shell_env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        ),
        ..Default::default()
    })
}

#[tokio::test]
async fn reads_writes_lists_and_removes_files_and_directories() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.absolute_path("nested/child", None).await.unwrap();
    assert_eq!(
        env.absolute_path("nested/child", None).await.unwrap(),
        format!("{root}/nested/child")
    );
    assert_eq!(
        env.join_path(
            &[root.to_string(), "nested".to_string(), "child".to_string()],
            None
        )
        .await
        .unwrap(),
        format!("{root}/nested/child")
    );
    env.create_dir("nested/child", CreateDirOptions::default(), None)
        .await
        .unwrap();
    env.write_file(
        "nested/child/file.txt",
        &WriteContent::Text("hel".to_string()),
        None,
    )
    .await
    .unwrap();
    env.append_file(
        "nested/child/file.txt",
        &WriteContent::Text("lo".to_string()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        env.read_text_file("nested/child/file.txt", None)
            .await
            .unwrap(),
        "hello"
    );
    assert_eq!(
        env.read_text_lines(
            "nested/child/file.txt",
            ReadTextLinesOptions { max_lines: Some(1) },
            None
        )
        .await
        .unwrap(),
        ["hello".to_string()]
    );
    assert_eq!(
        String::from_utf8(
            env.read_binary_file("nested/child/file.txt", None)
                .await
                .unwrap()
        )
        .unwrap(),
        "hello"
    );

    let entries = env.list_dir("nested/child", None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "file.txt");
    assert_eq!(entries[0].path, format!("{root}/nested/child/file.txt"));
    assert_eq!(entries[0].kind, FileKind::File);
    assert_eq!(entries[0].size, 5);

    assert!(env.exists("nested/child/file.txt", None).await.unwrap());
    env.remove("nested/child/file.txt", RemoveOptions::default(), None)
        .await
        .unwrap();
    assert!(!env.exists("nested/child/file.txt", None).await.unwrap());
}

#[tokio::test]
async fn expands_home_relative_paths_and_file_urls() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        env.absolute_path("~/pi-node-env-test", None).await.unwrap(),
        format!("{home}/pi-node-env-test")
    );
    let file_path = format!("{root}/file with spaces.txt");
    let url = format!(
        "file://{}",
        file_path
            .split('/')
            .map(|segment| segment.replace('%', "%25").replace(' ', "%20"))
            .collect::<Vec<_>>()
            .join("/")
    );
    assert_eq!(env.absolute_path(&url, None).await.unwrap(), file_path);
}

#[tokio::test]
async fn returns_file_info_without_following_symlinks() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.create_dir("dir", CreateDirOptions::default(), None)
        .await
        .unwrap();
    env.write_file(
        "dir/file.txt",
        &WriteContent::Text("hello".to_string()),
        None,
    )
    .await
    .unwrap();
    std::os::unix::fs::symlink(format!("{root}/dir/file.txt"), format!("{root}/file-link"))
        .unwrap();
    std::os::unix::fs::symlink(format!("{root}/dir"), format!("{root}/dir-link")).unwrap();

    let dir_info = env.file_info("dir", None).await.unwrap();
    assert_eq!(dir_info.name, "dir");
    assert_eq!(dir_info.path, format!("{root}/dir"));
    assert_eq!(dir_info.kind, FileKind::Directory);

    let file_info = env.file_info("dir/file.txt", None).await.unwrap();
    assert_eq!(file_info.name, "file.txt");
    assert_eq!(file_info.path, format!("{root}/dir/file.txt"));
    assert_eq!(file_info.kind, FileKind::File);
    assert_eq!(file_info.size, 5);

    let link_info = env.file_info("file-link", None).await.unwrap();
    assert_eq!(link_info.kind, FileKind::Symlink);
    let dir_link_info = env.file_info("dir-link", None).await.unwrap();
    assert_eq!(dir_link_info.kind, FileKind::Symlink);

    assert_eq!(
        env.canonical_path("file-link", None).await.unwrap(),
        std::fs::canonicalize(format!("{root}/dir/file.txt"))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );
}

#[tokio::test]
async fn lists_symlinks_as_symlinks() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file("target.txt", &WriteContent::Text("hello".to_string()), None)
        .await
        .unwrap();
    std::os::unix::fs::symlink(format!("{root}/target.txt"), format!("{root}/link.txt")).unwrap();

    let mut entries = env.list_dir(".", None).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let summarized: Vec<(&str, FileKind)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind))
        .collect();
    assert_eq!(
        summarized,
        [
            ("link.txt", FileKind::Symlink),
            ("target.txt", FileKind::File)
        ]
    );
}

#[tokio::test]
async fn stops_reading_text_lines_at_the_requested_limit() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file(
        "file.txt",
        &WriteContent::Text("one\ntwo\nthree".to_string()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        env.read_text_lines(
            "file.txt",
            ReadTextLinesOptions { max_lines: Some(1) },
            None
        )
        .await
        .unwrap(),
        ["one".to_string()]
    );
}

#[tokio::test]
async fn returns_file_error_for_missing_paths() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let error = env.file_info("missing.txt", None).await.unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::FileErrorCode::NotFound
    );
    assert_eq!(error.path, Some(format!("{root}/missing.txt")));
    assert!(!env.exists("missing.txt", None).await.unwrap());
}

#[tokio::test]
async fn returns_file_error_for_listing_non_directories() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file("file.txt", &WriteContent::Text("hello".to_string()), None)
        .await
        .unwrap();
    let error = env.list_dir("file.txt", None).await.unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::FileErrorCode::NotDirectory
    );
}

#[tokio::test]
async fn appends_to_new_files_and_creates_parent_directories() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.append_file(
        "new/nested/file.txt",
        &WriteContent::Text("a".to_string()),
        None,
    )
    .await
    .unwrap();
    env.append_file(
        "new/nested/file.txt",
        &WriteContent::Text("b".to_string()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        env.read_text_file("new/nested/file.txt", None)
            .await
            .unwrap(),
        "ab"
    );
}

#[tokio::test]
async fn atomically_renames_a_file_and_replaces_the_destination() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file("source.txt", &WriteContent::Text("new".to_string()), None)
        .await
        .unwrap();
    env.write_file(
        "destination.txt",
        &WriteContent::Text("old".to_string()),
        None,
    )
    .await
    .unwrap();

    env.rename_file("source.txt", "destination.txt", None)
        .await
        .unwrap();

    assert!(!env.exists("source.txt", None).await.unwrap());
    assert_eq!(
        env.read_text_file("destination.txt", None).await.unwrap(),
        "new"
    );
}

#[tokio::test]
async fn reports_the_source_path_when_rename_fails_because_the_source_is_missing() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file(
        "destination.txt",
        &WriteContent::Text("unchanged".to_string()),
        None,
    )
    .await
    .unwrap();

    let error = env
        .rename_file("missing-source.txt", "destination.txt", None)
        .await
        .unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::FileErrorCode::NotFound
    );
    assert_eq!(error.path, Some(format!("{root}/missing-source.txt")));
    assert_eq!(
        env.read_text_file("destination.txt", None).await.unwrap(),
        "unchanged"
    );
}

#[tokio::test]
async fn creates_temporary_directories_and_files() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let temp_dir = env.create_temp_dir("node-env-test-", None).await.unwrap();
    assert!(std::path::Path::new(&temp_dir).exists());
    let temp_file = env
        .create_temp_file(
            &CreateTempFileOptions {
                prefix: Some("prefix-".to_string()),
                suffix: Some(".txt".to_string()),
            },
            None,
        )
        .await
        .unwrap();
    assert!(std::path::Path::new(&temp_file).exists());
    assert!(temp_file.ends_with(".txt"));
}

#[tokio::test]
async fn honors_create_dir_recursive_false_and_remove_options() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let error = env
        .create_dir(
            "missing/child",
            CreateDirOptions {
                recursive: Some(false),
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::FileErrorCode::NotFound
    );

    env.write_file(
        "dir/child/file.txt",
        &WriteContent::Text("hello".to_string()),
        None,
    )
    .await
    .unwrap();
    assert!(
        env.remove(
            "dir",
            RemoveOptions {
                recursive: Some(false),
                force: None,
            },
            None
        )
        .await
        .is_err()
    );
    env.remove(
        "dir",
        RemoveOptions {
            recursive: Some(true),
            force: None,
        },
        None,
    )
    .await
    .unwrap();
    assert!(!env.exists("dir", None).await.unwrap());

    assert!(
        env.remove(
            "missing",
            RemoveOptions {
                recursive: None,
                force: Some(false),
            },
            None
        )
        .await
        .is_err()
    );
    env.remove(
        "missing",
        RemoveOptions {
            recursive: None,
            force: Some(true),
        },
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn returns_aborted_results_for_pre_aborted_cancellable_file_operations() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    env.write_file("file.txt", &WriteContent::Text("hello".to_string()), None)
        .await
        .unwrap();
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();

    let results: Vec<pi_core::agent::harness::types::FileErrorCode> = vec![
        env.read_text_file("file.txt", Some(signal.clone()))
            .await
            .map(|_| ())
            .unwrap_err()
            .code,
        env.read_text_lines(
            "file.txt",
            ReadTextLinesOptions::default(),
            Some(signal.clone()),
        )
        .await
        .map(|_| ())
        .unwrap_err()
        .code,
        env.read_binary_file("file.txt", Some(signal.clone()))
            .await
            .map(|_| ())
            .unwrap_err()
            .code,
        env.write_file(
            "other.txt",
            &WriteContent::Text("hello".to_string()),
            Some(signal.clone()),
        )
        .await
        .unwrap_err()
        .code,
        env.rename_file("file.txt", "renamed.txt", Some(signal.clone()))
            .await
            .unwrap_err()
            .code,
        env.list_dir(".", Some(signal.clone()))
            .await
            .map(|_| ())
            .unwrap_err()
            .code,
    ];
    for code in results {
        assert_eq!(code, pi_core::agent::harness::types::FileErrorCode::Aborted);
    }
}

#[tokio::test]
async fn cleanup_is_best_effort() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    FileSystem::cleanup(&env).await;
}

#[tokio::test]
async fn executes_commands_in_cwd_with_env_overrides() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("NODE_ENV_TEST".to_string(), "ok".to_string());
    let result = env
        .exec(
            "printf '%s:%s' \"$PWD\" \"$NODE_ENV_TEST\"",
            Some(&ShellExecOptions {
                env: Some(overrides),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let canonical = std::fs::canonicalize(root.as_ref())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(result.stdout, format!("{canonical}:ok"));
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn applies_string_shell_environment_overrides() {
    // (description, overrides, expected session file)
    type OverrideCase = (
        &'static str,
        Option<Vec<(&'static str, &'static str)>>,
        &'static str,
    );
    let cases: Vec<OverrideCase> = vec![
        (
            "a missing override preserves the base value",
            None,
            "x:/stale/parent.jsonl",
        ),
        (
            "an empty override shadows the base value",
            Some(vec![("PI_SESSION_FILE", "")]),
            "x:",
        ),
        (
            "a string override replaces the base value",
            Some(vec![("PI_SESSION_FILE", "/sessions/current.jsonl")]),
            "x:/sessions/current.jsonl",
        ),
    ];
    for (_, overrides, expected_session_file) in cases {
        let root = common::create_temp_dir();
        let env = shell_env_for(
            &root,
            vec![
                ("PI_SESSION_FILE", "/stale/parent.jsonl"),
                ("PI_CODING_AGENT", "true"),
                ("PI_NODE_ENV_PRESERVED_TEST", "preserved"),
            ],
        );
        let overrides = overrides.map(|pairs| {
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<std::collections::BTreeMap<String, String>>()
        });
        let result = env
            .exec(
                "printf '%s:%s|%s|%s' \"${PI_SESSION_FILE+x}\" \"${PI_SESSION_FILE-}\" \"$PI_CODING_AGENT\" \"$PI_NODE_ENV_PRESERVED_TEST\"",
                Some(&ShellExecOptions {
                    env: overrides,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            result.stdout,
            format!("{expected_session_file}|true|preserved")
        );
    }
}

#[tokio::test]
async fn can_replace_rather_than_inherit_the_default_shell_environment() {
    let root = common::create_temp_dir();
    let inherited_key = "PI_NODE_ENV_INHERITED_TEST";
    let configured_key = "PI_NODE_ENV_CONFIGURED_TEST";
    let explicit_key = "PI_NODE_ENV_EXPLICIT_TEST";

    let env = shell_env_for(&root, vec![(configured_key, "configured")]);
    let mut explicit = std::collections::BTreeMap::new();
    explicit.insert(explicit_key.to_string(), "explicit".to_string());
    // SAFETY: isolated per-test key; no other test in this binary reads or
    // writes it while this test runs.
    unsafe { std::env::set_var(inherited_key, "host") };
    let result = env
        .exec(
            &format!(
                "printf '%s:%s:%s' \"${{{inherited_key}-}}\" \"${{{configured_key}-}}\" \"${{{explicit_key}-}}\""
            ),
            Some(&ShellExecOptions {
                inherit_env: Some(false),
                env: Some(explicit),
                ..Default::default()
            }),
        )
        .await;
    unsafe { std::env::remove_var(inherited_key) };

    assert_eq!(result.unwrap().stdout, "::explicit");
}

#[tokio::test]
async fn cleanup_terminates_active_shell_processes() {
    let root = common::create_temp_dir();
    let env = Arc::new(env_for(&root));
    let exec_env = Arc::clone(&env);
    let mut execution =
        tokio::spawn(async move { exec_env.exec("touch started; sleep 60", None).await });
    let mut attempts = 0;
    while attempts < 100 && !env.exists("started", None).await.unwrap() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        attempts += 1;
    }
    assert!(env.exists("started", None).await.unwrap());
    FileSystem::cleanup(env.as_ref()).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), &mut execution).await;
    assert!(result.unwrap().is_ok());
}

#[tokio::test]
async fn streams_stdout_and_stderr_chunks() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let stdout = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr = Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_listener: pi_core::agent::harness::types::ChunkListener = {
        let stdout = Arc::clone(&stdout);
        Arc::new(move |chunk: &str| {
            stdout.lock().unwrap().push_str(chunk);
            Ok(())
        })
    };
    let stderr_listener: pi_core::agent::harness::types::ChunkListener = {
        let stderr = Arc::clone(&stderr);
        Arc::new(move |chunk: &str| {
            stderr.lock().unwrap().push_str(chunk);
            Ok(())
        })
    };
    let result = env
        .exec(
            "printf out; printf err >&2",
            Some(&ShellExecOptions {
                on_stdout: Some(stdout_listener),
                on_stderr: Some(stderr_listener),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "out");
    assert_eq!(result.stderr, "err");
    assert_eq!(result.exit_code, 0);
    assert_eq!(*stdout.lock().unwrap(), "out");
    assert_eq!(*stderr.lock().unwrap(), "err");
}

#[tokio::test]
async fn reports_a_missing_working_directory_before_spawning() {
    let root = common::create_temp_dir();
    let env = env_for(&format!("{root}/missing"));
    let error = env.exec("printf ok", None).await.unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::ExecutionErrorCode::SpawnError
    );
    assert!(
        error.message.contains("Working directory does not exist"),
        "unexpected message: {}",
        error.message
    );
}

#[tokio::test]
async fn returns_non_zero_command_exit_codes_as_successful_execution_results() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let result = env.exec("exit 7", None).await.unwrap();
    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 7);
}

#[tokio::test]
async fn returns_timeout_errors_for_commands_exceeding_the_timeout() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let error = env
        .exec(
            "sleep 5",
            Some(&ShellExecOptions {
                timeout: Some(0.01),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::ExecutionErrorCode::Timeout
    );
}

#[tokio::test]
async fn returns_callback_errors_from_exec_stream_handlers() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let failing: pi_core::agent::harness::types::ChunkListener =
        Arc::new(|_chunk: &str| Err("callback failed".to_string()));
    let error = env
        .exec(
            "printf out",
            Some(&ShellExecOptions {
                on_stdout: Some(failing),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::ExecutionErrorCode::CallbackError
    );
    assert_eq!(error.message, "callback failed");
}

#[tokio::test]
async fn returns_shell_unavailable_and_spawn_errors() {
    let root = common::create_temp_dir();
    let missing_shell_env = NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        shell_path: Some(format!("{root}/missing-shell")),
        ..Default::default()
    });
    let missing_shell = missing_shell_env.exec("printf ok", None).await.unwrap_err();
    assert_eq!(
        missing_shell.code,
        pi_core::agent::harness::types::ExecutionErrorCode::ShellUnavailable
    );

    let env = env_for(&root);
    let shell_path = format!("{root}/not-executable-shell");
    env.write_file(
        &shell_path,
        &WriteContent::Text("not executable".to_string()),
        None,
    )
    .await
    .unwrap();
    let spawn_error_env = NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        shell_path: Some(shell_path),
        ..Default::default()
    });
    let spawn_error = spawn_error_env.exec("printf ok", None).await.unwrap_err();
    assert_eq!(
        spawn_error.code,
        pi_core::agent::harness::types::ExecutionErrorCode::SpawnError
    );
}

#[tokio::test]
async fn returns_an_aborted_result_for_aborted_commands() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let signal = tokio_util::sync::CancellationToken::new();
    let options = ShellExecOptions {
        abort_signal: Some(signal.clone()),
        ..Default::default()
    };
    let execution = env.exec("sleep 5", Some(&options));
    signal.cancel();
    let error = execution.await.unwrap_err();
    assert_eq!(
        error.code,
        pi_core::agent::harness::types::ExecutionErrorCode::Aborted
    );
}

#[tokio::test]
async fn captures_large_shell_output_to_a_full_output_file_through_the_execution_env() {
    let root = common::create_temp_dir();
    let env = env_for(&root);
    let result = execute_shell_with_capture(&env, "yes line | head -n 15000", None)
        .await
        .unwrap();
    assert!(result.truncated);
    let full_output_path = result.full_output_path.expect("full output file");
    let full_output = env.read_text_file(&full_output_path, None).await.unwrap();
    assert!(full_output.split('\n').count() > 10_000);
    assert!(result.output.len() < full_output.len());
}
