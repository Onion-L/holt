//! Port of `pi-core/agent/test/harness/tools.test.ts`.
//!
//! The mutation-queue blocking cases run through a wrapper env that parks
//! the first write until the test releases it (the TypeScript subclass
//! overrides); late-output bash cases use a stub env overriding `exec`.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::tools::bash::{BashExecution, BashToolOptions, create_bash_tool};
use pi_core::agent::harness::tools::edit::create_edit_tool;
use pi_core::agent::harness::tools::read::{ReadToolOptions, create_read_tool};
use pi_core::agent::harness::tools::tool_context::ExecutionToolContext;
use pi_core::agent::harness::tools::write::create_write_tool;
use pi_core::agent::harness::types::{
    AgentToolContext, ExecutionEnv, FileSystem, Shell, ShellExecResult, WriteContent,
};
use pi_core::agent::types::AgentToolResult;
use pi_core::ai::types::BlockContent;
use tokio_util::sync::CancellationToken;

fn context_for(root: &str) -> AgentToolContext {
    ExecutionToolContext {
        env: Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
            cwd: root.to_string(),
            ..Default::default()
        })),
    }
    .into_tool_context()
}

fn env_of(context: &AgentToolContext) -> &Arc<dyn ExecutionEnv> {
    &context
        .downcast_ref::<ExecutionToolContext>()
        .expect("execution context")
        .env
}

fn text_output(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run(
    tool: &pi_core::agent::harness::types::AgentHarnessTool,
    params: serde_json::Value,
    context: &AgentToolContext,
) -> Result<AgentToolResult, String> {
    ((tool.execute)("call-1", &params, None, None, context)).await
}

async fn write_file(env: &Arc<dyn ExecutionEnv>, path: &str, content: &str) {
    env.write_file(path, &WriteContent::Text(content.to_string()), None)
        .await
        .expect("write file");
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_reads_text_with_offsets_limits_and_continuation_notices() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let content = (1..=100)
        .map(|index| format!("Line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(env_of(&context), "test.txt", &content).await;

    let result = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "test.txt", "offset": 41, "limit": 20 }),
        &context,
    )
    .await
    .unwrap();
    let output = text_output(&result);

    assert!(!output.contains("Line 40"));
    assert!(output.contains("Line 41"));
    assert!(output.contains("Line 60"));
    assert!(!output.contains("Line 61"));
    assert!(output.contains("[40 more lines in file. Use offset=61 to continue.]"));
}

#[tokio::test]
async fn read_truncates_large_text_by_line_count() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let content = (1..=2500)
        .map(|index| format!("Line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(env_of(&context), "large.txt", &content).await;

    let result = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "large.txt" }),
        &context,
    )
    .await
    .unwrap();

    assert!(
        text_output(&result)
            .contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]")
    );
    let truncation = result
        .details
        .get("truncation")
        .expect("truncation details");
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["totalLines"], json!(2500));
    assert_eq!(truncation["outputLines"], json!(2000));
}

#[tokio::test]
async fn read_does_not_count_a_trailing_newline_as_an_extra_line_at_the_limit() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let content = format!("{}\n", vec!["x"; 2000].join("\n"));
    write_file(env_of(&context), "exact.txt", &content).await;

    let result = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "exact.txt" }),
        &context,
    )
    .await
    .unwrap();

    assert_eq!(result.details, serde_json::Value::Null);
    assert!(!text_output(&result).contains("Use offset="));
}

// Deliberate deviation from TypeScript v0.84.4 (see `ReadToolOptions`): the
// read tool accepts configurable truncation limits. TypeScript hardcodes
// 2000 lines / 50KB and leaves customization to extensions that replace the
// tool (earendil-works/pi#7066). Defaults must stay byte-identical.
#[tokio::test]
async fn read_honors_configurable_truncation_limits_and_defaults_unchanged() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let content = (1..=10)
        .map(|index| format!("0123456789-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(env_of(&context), "sized.txt", &content).await;

    // Line limit override.
    let tool = create_read_tool(ReadToolOptions {
        max_lines: Some(3),
        max_bytes: Some(64 * 1024),
        ..Default::default()
    });
    let result = run(&tool, json!({ "path": "sized.txt" }), &context)
        .await
        .unwrap();
    let output = text_output(&result);
    assert!(output.contains("[Showing lines 1-3 of 10. Use offset=4 to continue.]"));
    assert_eq!(result.details["truncation"]["maxLines"], json!(3));
    assert_eq!(result.details["truncation"]["maxBytes"], json!(64 * 1024));
    assert!(tool.description.contains("truncated to 3 lines or 64KB"));

    // Byte limit override: each "0123456789-N" line is 12-13 bytes plus the
    // joining newline, so 25 bytes keeps exactly the first two lines.
    let tool = create_read_tool(ReadToolOptions {
        max_bytes: Some(25),
        ..Default::default()
    });
    let result = run(&tool, json!({ "path": "sized.txt" }), &context)
        .await
        .unwrap();
    let output = text_output(&result);
    assert!(output.contains("[Showing lines 1-2 of 10 (25B limit). Use offset=3 to continue.]"));
    assert_eq!(result.details["truncation"]["truncatedBy"], json!("bytes"));

    // Defaults are untouched: the same file reads in full with no notices.
    let result = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "sized.txt" }),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(result.details, serde_json::Value::Null);
    assert_eq!(text_output(&result), content);
    assert!(
        !create_read_tool(ReadToolOptions::default())
            .description
            .contains("truncated to 3 lines")
    );
}

#[tokio::test]
async fn read_rejects_offsets_beyond_the_file() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "short.txt", "one\ntwo\nthree").await;

    let error = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "short.txt", "offset": 100 }),
        &context,
    )
    .await
    .unwrap_err();
    assert_eq!(error, "Offset 100 is beyond end of file (3 lines total)");
}

#[tokio::test]
async fn read_detects_supported_images_by_content() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let png: Vec<u8> = BASE64_PNG.to_vec();
    env_of(&context)
        .write_file("image.txt", &WriteContent::Bytes(png.clone()), None)
        .await
        .unwrap();

    let result = run(
        &create_read_tool(ReadToolOptions::default()),
        json!({ "path": "image.txt" }),
        &context,
    )
    .await
    .unwrap();

    assert!(text_output(&result).contains("Read image file [image/png]"));
    let expected_data = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&png)
    };
    assert!(
        result.content.iter().any(|block| matches!(block,
            BlockContent::Image(image) if image.data == expected_data && image.mime_type == "image/png"))
    );
}

const BASE64_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_writes_files_and_creates_parent_directories() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let result = run(
        &create_write_tool(),
        json!({ "path": "nested/dir/file.txt", "content": "hello" }),
        &context,
    )
    .await
    .unwrap();

    assert_eq!(
        text_output(&result),
        "Successfully wrote 5 bytes to nested/dir/file.txt"
    );
    assert_eq!(
        env_of(&context)
            .read_text_file("nested/dir/file.txt", None)
            .await
            .unwrap(),
        "hello"
    );
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_applies_disjoint_edits_and_returns_both_diff_formats() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let original = "alpha\nbeta\ngamma\ndelta\n";
    write_file(env_of(&context), "edit.txt", original).await;

    let result = run(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [
            { "oldText": "alpha\n", "newText": "ALPHA\n" },
            { "oldText": "gamma\n", "newText": "GAMMA\n" },
        ]}),
        &context,
    )
    .await
    .unwrap();

    assert_eq!(
        text_output(&result),
        "Successfully replaced 2 block(s) in edit.txt."
    );
    let diff = result.details["diff"].as_str().unwrap();
    assert!(diff.contains("ALPHA"));
    assert!(diff.contains("GAMMA"));
    // The unified patch matches the npm `diff` package rendering for the
    // same inputs (header-only file headers, always-printed counts).
    assert_eq!(
        result.details["patch"].as_str().unwrap(),
        "--- edit.txt\n+++ edit.txt\n@@ -1,4 +1,4 @@\n-alpha\n+ALPHA\n beta\n-gamma\n+GAMMA\n delta\n"
    );
    assert_eq!(
        env_of(&context)
            .read_text_file("edit.txt", None)
            .await
            .unwrap(),
        "ALPHA\nbeta\nGAMMA\ndelta\n"
    );
}

#[tokio::test]
async fn edit_matches_all_edits_against_the_original_and_rejects_overlaps() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "edit.txt", "one\ntwo\nthree\n").await;

    let error = run(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [
            { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
            { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" },
        ]}),
        &context,
    )
    .await
    .unwrap_err();
    assert!(error.contains("overlap"), "unexpected error: {error}");
    assert_eq!(
        env_of(&context)
            .read_text_file("edit.txt", None)
            .await
            .unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[tokio::test]
async fn edit_rejects_missing_and_duplicate_target_text() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "edit.txt", "foo foo foo").await;
    let tool = create_edit_tool();

    let error = run(
        &tool,
        json!({ "path": "edit.txt", "edits": [{ "oldText": "bar", "newText": "baz" }] }),
        &context,
    )
    .await
    .unwrap_err();
    assert!(error.contains("Could not find the exact text"), "{error}");
    let error = run(
        &tool,
        json!({ "path": "edit.txt", "edits": [{ "oldText": "foo", "newText": "bar" }] }),
        &context,
    )
    .await
    .unwrap_err();
    assert!(error.contains("Found 3 occurrences"), "{error}");
}

// Deliberate deviation from TypeScript v0.84.4 (see the diagnostics block in
// `edit_diff.rs`): when an oldText is not found, the Rust error appends the
// closest candidate region so the model can self-correct in one retry. When
// nothing is close, the message stays byte-identical to TypeScript.
#[tokio::test]
async fn edit_not_found_includes_closest_candidate_hint() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(
        env_of(&context),
        "indent.rs",
        "fn main() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .await;

    // Classic model failure: oldText uses tabs where the file has 4-space
    // indentation. Neither exact nor fuzzy matching tolerates that.
    let error = run(
        &create_edit_tool(),
        json!({ "path": "indent.rs", "edits": [
            { "oldText": "\tlet a = 1;\n\tlet b = 2;\n", "newText": "\tlet a = 2;\n\tlet b = 3;\n" }
        ]}),
        &context,
    )
    .await
    .unwrap_err();

    assert!(
        error.starts_with(
            "Could not find the exact text in indent.rs. The old text must match exactly including all whitespace and newlines."
        ),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("The closest match in the file is around lines 2-3:"),
        "{error}"
    );
    assert!(error.contains("2 |     let a = 1;"), "{error}");
    assert!(error.contains("3 |     let b = 2;"), "{error}");
    assert!(error.contains("Re-read that region"), "{error}");
    assert_eq!(
        env_of(&context)
            .read_text_file("indent.rs", None)
            .await
            .unwrap(),
        "fn main() {\n    let a = 1;\n    let b = 2;\n}\n"
    );
}

#[tokio::test]
async fn edit_not_found_without_a_close_candidate_is_byte_identical_to_typescript() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "edit.txt", "foo foo foo").await;

    let error = run(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [{ "oldText": "unrelated zebra", "newText": "x" }] }),
        &context,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        "Could not find the exact text in edit.txt. The old text must match exactly including all whitespace and newlines."
    );
}

#[tokio::test]
async fn edit_not_found_diagnoses_sequential_edit_assumption() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "edit.txt", "one\ntwo\nthree\n").await;

    // edits[1] was written assuming edits[0] had already been applied.
    let error = run(
        &create_edit_tool(),
        json!({ "path": "edit.txt", "edits": [
            { "oldText": "one\n", "newText": "ONE\n" },
            { "oldText": "ONE\ntwo\n", "newText": "ONE\nTWO\n" }
        ]}),
        &context,
    )
    .await
    .unwrap_err();

    assert!(
        error.starts_with(
            "Could not find edits[1] in edit.txt. The oldText must match exactly including all whitespace and newlines."
        ),
        "unexpected error: {error}"
    );
    assert!(
        error.contains(
            "edits[1].oldText matches only after edits[0..1] are applied in order, but every edit is matched against the same original file content"
        ),
        "unexpected error: {error}"
    );
    assert!(error.contains("Merge the dependent edits"), "{error}");
    assert_eq!(
        env_of(&context)
            .read_text_file("edit.txt", None)
            .await
            .unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[tokio::test]
async fn edit_serializes_concurrent_edits_through_canonical_and_symlink_paths() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "real.txt", "value\n").await;
    std::os::unix::fs::symlink(format!("{root}/real.txt"), format!("{root}/link.txt")).unwrap();

    let tool = create_edit_tool();
    let first = run(
        &tool,
        json!({ "path": "real.txt", "edits": [{ "oldText": "value", "newText": "first" }] }),
        &context,
    );
    let second = run(
        &tool,
        json!({ "path": "link.txt", "edits": [{ "oldText": "value", "newText": "second" }] }),
        &context,
    );
    let (first, second) = tokio::join!(first, second);
    // Both apply sequentially; the second edit finds the file changed by
    // the first, so exactly one edit succeeds against "value".
    assert!(first.is_ok() || second.is_ok());
    let final_content = env_of(&context)
        .read_text_file("real.txt", None)
        .await
        .unwrap();
    assert!(final_content == "first\n" || final_content == "second\n");
}

#[tokio::test]
async fn edit_preserves_bom_and_crlf_line_endings() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "crlf.txt", "\u{feff}alpha\r\nbeta\r\n").await;

    let result = run(
        &create_edit_tool(),
        json!({ "path": "crlf.txt", "edits": [{ "oldText": "alpha\n", "newText": "ALPHA\n" }] }),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(
        text_output(&result),
        "Successfully replaced 1 block(s) in crlf.txt."
    );
    let written = env_of(&context)
        .read_binary_file("crlf.txt", None)
        .await
        .unwrap();
    let text = String::from_utf8(written).unwrap();
    assert_eq!(text, "\u{feff}ALPHA\r\nbeta\r\n");
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_executes_commands_and_combines_stdout_and_stderr() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let result = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "printf out; printf err >&2" }),
        &context,
    )
    .await
    .unwrap();
    let output = text_output(&result);
    assert!(output.contains("out"));
    assert!(output.contains("err"));
}

#[tokio::test]
async fn bash_reports_nonzero_exits_and_timeouts() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let error = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "exit 3" }),
        &context,
    )
    .await
    .unwrap_err();
    assert!(error.contains("Command exited with code 3"), "{error}");

    let error = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "sleep 5", "timeout": 0.01 }),
        &context,
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("Command timed out after 0.01 seconds"),
        "{error}"
    );
}

#[tokio::test]
async fn bash_preserves_truncated_output_when_a_command_times_out() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let lines = (1..=pi_core::agent::harness::utils::truncate::DEFAULT_MAX_LINES + 1)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(
        env_of(&context),
        "slow-output.sh",
        &format!("printf '%s\\n' \"{lines}\"\nsleep 5\n"),
    )
    .await;
    std::fs::set_permissions(
        format!("{root}/slow-output.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let error = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "./slow-output.sh", "timeout": 0.2 }),
        &context,
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("Command timed out after 0.2 seconds"),
        "{error}"
    );
    assert!(
        error.contains("line-1\n") || error.contains("line-2\n"),
        "truncated tail kept: {error}"
    );
    assert!(error.contains("Full output:"), "{error}");
}

#[tokio::test]
async fn bash_supports_command_prefixes_and_prepare_hooks() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let prepare: pi_core::agent::harness::tools::bash::BashPrepare = Arc::new(
        |execution: &mut BashExecution, _signal: Option<&CancellationToken>| {
            let execution: &mut BashExecution = execution;
            execution
                .env
                .insert("PREPARED".to_string(), "yes".to_string());
            Box::pin(async move {})
        },
    );
    let tool = create_bash_tool(BashToolOptions {
        command_prefix: Some("echo prefixed >&2".to_string()),
        prepare: Some(prepare),
    });
    let result = run(
        &tool,
        json!({ "command": "printf '%s' \"$PREPARED\"" }),
        &context,
    )
    .await
    .unwrap();
    let output = text_output(&result);
    assert!(output.contains("prefixed"), "{output}");
    assert!(output.contains("yes"), "{output}");
}

#[tokio::test]
async fn bash_reports_the_total_size_of_an_oversized_final_line() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let big_line = "X".repeat(300_000);
    write_file(
        env_of(&context),
        "big-line.sh",
        &format!("printf '%s' '{big_line}'\n"),
    )
    .await;
    std::fs::set_permissions(
        format!("{root}/big-line.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let result = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "./big-line.sh" }),
        &context,
    )
    .await
    .unwrap();
    let output = text_output(&result);
    assert!(
        output.contains("(line is 293.0KB). Full output:"),
        "unexpected output tail: {}",
        &output[output.len().saturating_sub(400)..]
    );
    assert!(
        output.contains("[Showing last 50.0KB of line 1"),
        "{output}"
    );
}

// ---------------------------------------------------------------------------
// Shell stub for late-output behavior
// ---------------------------------------------------------------------------

struct StubEnv {
    inner: NodeExecutionEnv,
}

impl FileSystem for StubEnv {
    fn cwd(&self) -> String {
        self.inner.cwd()
    }
    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.absolute_path(path, signal)
    }
    fn join_path<'a>(
        &'a self,
        parts: &'a [String],
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.join_path(parts, signal)
    }
    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.read_text_file(path, signal)
    }
    fn read_text_lines<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::ReadTextLinesOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<Vec<String>, pi_core::agent::harness::types::FileError>,
    > {
        self.inner.read_text_lines(path, options, signal)
    }
    fn read_binary_file<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, pi_core::agent::harness::types::FileError>>
    {
        self.inner.read_binary_file(path, signal)
    }
    fn write_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.write_file(path, content, signal)
    }
    fn append_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.append_file(path, content, signal)
    }
    fn rename_file<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.rename_file(source, destination, signal)
    }
    fn file_info<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<pi_core::agent::harness::types::FileInfo, pi_core::agent::harness::types::FileError>,
    > {
        self.inner.file_info(path, signal)
    }
    fn list_dir<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<
            Vec<pi_core::agent::harness::types::FileInfo>,
            pi_core::agent::harness::types::FileError,
        >,
    > {
        self.inner.list_dir(path, signal)
    }
    fn canonical_path<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.canonical_path(path, signal)
    }
    fn exists<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<bool, pi_core::agent::harness::types::FileError>>
    {
        self.inner.exists(path, signal)
    }
    fn create_dir<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::CreateDirOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.create_dir(path, options, signal)
    }
    fn remove<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::RemoveOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.remove(path, options, signal)
    }
    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.create_temp_dir(prefix, signal)
    }
    fn create_temp_file<'a>(
        &'a self,
        options: &'a pi_core::agent::harness::types::CreateTempFileOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.create_temp_file(options, signal)
    }
    fn cleanup(&self) -> futures::future::BoxFuture<'static, ()> {
        FileSystem::cleanup(&self.inner)
    }
}

impl Shell for StubEnv {
    fn exec<'a>(
        &'a self,
        _command: &'a str,
        options: Option<&'a pi_core::agent::harness::types::ShellExecOptions>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<ShellExecResult, pi_core::agent::harness::types::ExecutionError>,
    > {
        Box::pin(async move {
            let on_stdout = options.and_then(|options| options.on_stdout.clone());
            if let Some(on_stdout) = &on_stdout {
                let _ = on_stdout("before\n");
            }
            // The late chunk fires after exec resolves (setTimeout(0));
            // the capture layer ignores it once execution settles.
            if let Some(on_stdout) = on_stdout.clone() {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let _ = on_stdout("late\n");
                });
            }
            Ok(ShellExecResult {
                stdout: "before\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
            })
        })
    }
    fn cleanup(&self) -> futures::future::BoxFuture<'static, ()> {
        Shell::cleanup(&self.inner)
    }
}

#[tokio::test]
async fn bash_ignores_output_callbacks_after_execution_settles() {
    let root = common::create_temp_dir();
    let env: Arc<dyn ExecutionEnv> = Arc::new(StubEnv {
        inner: NodeExecutionEnv::new(NodeExecutionEnvOptions {
            cwd: root.to_string(),
            ..Default::default()
        }),
    });
    let context = ExecutionToolContext { env }.into_tool_context();
    let result = run(
        &create_bash_tool(BashToolOptions::default()),
        json!({ "command": "anything" }),
        &context,
    )
    .await
    .unwrap();
    let output = text_output(&result);
    assert!(output.contains("before"), "{output}");
    assert!(!output.contains("late"), "{output}");
}

/// A wrapper that parks the first observed `write_file` payload until the
/// test releases it (the TS `BlockingWriteExecutionEnv` /
/// `BlockingEditExecutionEnv` subclasses): `first_content` parks, any later
/// write sets `second_started`.
struct BlockingWriteEnv {
    inner: NodeExecutionEnv,
    first_content: String,
    first_started: tokio::sync::watch::Sender<bool>,
    release: Arc<tokio::sync::Semaphore>,
    second_content: String,
    second_started: AtomicBool,
    writes: Mutex<Vec<String>>,
}

impl FileSystem for BlockingWriteEnv {
    fn cwd(&self) -> String {
        self.inner.cwd()
    }
    fn absolute_path<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.absolute_path(path, signal)
    }
    fn join_path<'a>(
        &'a self,
        parts: &'a [String],
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.join_path(parts, signal)
    }
    fn read_text_file<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.read_text_file(path, signal)
    }
    fn read_text_lines<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::ReadTextLinesOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<Vec<String>, pi_core::agent::harness::types::FileError>,
    > {
        self.inner.read_text_lines(path, options, signal)
    }
    fn read_binary_file<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, pi_core::agent::harness::types::FileError>>
    {
        self.inner.read_binary_file(path, signal)
    }
    fn write_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        let is_first = match content {
            WriteContent::Text(text) => text == &self.first_content,
            _ => false,
        };
        let inner = &self.inner;
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            if is_first {
                let _ = self.first_started.send(true);
                let _ = release.acquire().await;
            } else if let WriteContent::Text(text) = content
                && text == &self.second_content
            {
                self.second_started.store(true, Ordering::SeqCst);
            }
            if let WriteContent::Text(text) = content {
                self.writes.lock().unwrap().push(text.clone());
            }
            // The park sits inside the real write (past the pre-checks), and
            // Node's fs.writeFile ignores its signal option, so the parked
            // write still lands after release even when the signal aborted.
            let _ = signal;
            inner.write_file(path, content, None).await
        })
    }
    fn append_file<'a>(
        &'a self,
        path: &'a str,
        content: &'a WriteContent,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.append_file(path, content, signal)
    }
    fn rename_file<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.rename_file(source, destination, signal)
    }
    fn file_info<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<pi_core::agent::harness::types::FileInfo, pi_core::agent::harness::types::FileError>,
    > {
        self.inner.file_info(path, signal)
    }
    fn list_dir<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<
            Vec<pi_core::agent::harness::types::FileInfo>,
            pi_core::agent::harness::types::FileError,
        >,
    > {
        self.inner.list_dir(path, signal)
    }
    fn canonical_path<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.canonical_path(path, signal)
    }
    fn exists<'a>(
        &'a self,
        path: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<bool, pi_core::agent::harness::types::FileError>>
    {
        self.inner.exists(path, signal)
    }
    fn create_dir<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::CreateDirOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.create_dir(path, options, signal)
    }
    fn remove<'a>(
        &'a self,
        path: &'a str,
        options: pi_core::agent::harness::types::RemoveOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<(), pi_core::agent::harness::types::FileError>> {
        self.inner.remove(path, options, signal)
    }
    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.create_temp_dir(prefix, signal)
    }
    fn create_temp_file<'a>(
        &'a self,
        options: &'a pi_core::agent::harness::types::CreateTempFileOptions,
        signal: Option<CancellationToken>,
    ) -> futures::future::BoxFuture<'a, Result<String, pi_core::agent::harness::types::FileError>>
    {
        self.inner.create_temp_file(options, signal)
    }
    fn cleanup(&self) -> futures::future::BoxFuture<'static, ()> {
        FileSystem::cleanup(&self.inner)
    }
}

impl Shell for BlockingWriteEnv {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        options: Option<&'a pi_core::agent::harness::types::ShellExecOptions>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<ShellExecResult, pi_core::agent::harness::types::ExecutionError>,
    > {
        self.inner.exec(command, options)
    }
    fn cleanup(&self) -> futures::future::BoxFuture<'static, ()> {
        Shell::cleanup(&self.inner)
    }
}

async fn wait_first_started(env: &BlockingWriteEnv) {
    let mut rx = env.first_started.subscribe();
    if *rx.borrow_and_update() {
        return;
    }
    rx.changed()
        .await
        .expect("first write starts before the wait is abandoned");
}

fn blocking_context(env: Arc<BlockingWriteEnv>) -> AgentToolContext {
    ExecutionToolContext { env }.into_tool_context()
}

async fn run_with_signal(
    tool: &pi_core::agent::harness::types::AgentHarnessTool,
    id: &str,
    params: serde_json::Value,
    signal: Option<&CancellationToken>,
    context: &AgentToolContext,
) -> Result<AgentToolResult, String> {
    ((tool.execute)(id, &params, signal, None, context)).await
}

/// Port of "delegates image conversion and resizing to an injected
/// processor".
#[tokio::test]
async fn read_delegates_image_conversion_and_resizing_to_an_injected_processor() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    // createTinyBmp: a 58-byte 1x1 24-bit BMP.
    let mut bmp = vec![0u8; 58];
    bmp[0] = 0x42;
    bmp[1] = 0x4d;
    bmp[2..6].copy_from_slice(&58u32.to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
    env_of(&context)
        .write_file("image.bmp", &WriteContent::Bytes(bmp.clone()), None)
        .await
        .expect("write bmp");

    type Seen = (Vec<u8>, String, bool);
    let received: Arc<Mutex<Option<Seen>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&received);
    let tool = create_read_tool(ReadToolOptions {
        auto_resize_images: Some(false),
        image_processor: Some(Arc::new(move |bytes, mime_type, auto_resize| {
            let seen = Arc::clone(&seen);
            let bytes = bytes.to_vec();
            let mime_type = mime_type.to_string();
            Box::pin(async move {
                *seen.lock().unwrap() = Some((bytes, mime_type, auto_resize));
                pi_core::agent::harness::tools::read::ReadImageProcessorResult::Ok {
                    data: "converted".to_string(),
                    mime_type: "image/png".to_string(),
                    hints: vec!["[Image converted from image/bmp to image/png.]".to_string()],
                }
            })
        })),
        ..Default::default()
    });

    let result = run(&tool, json!({ "path": "image.bmp" }), &context)
        .await
        .unwrap();

    let (bytes, mime_type, auto_resize) =
        received.lock().unwrap().clone().expect("processor called");
    assert_eq!(mime_type, "image/bmp");
    assert!(!auto_resize);
    assert_eq!(bytes, bmp);
    assert!(
        text_output(&result).contains("[Image converted from image/bmp to image/png.]"),
        "missing hint: {}",
        text_output(&result)
    );
    assert!(result.content.iter().any(|block| match block {
        BlockContent::Image(image) => {
            image.data == "converted" && image.mime_type == "image/png"
        }
        _ => false,
    }));
}

/// Port of "keeps the mutation queue locked until an aborted write settles".
#[tokio::test]
async fn keeps_the_mutation_queue_locked_until_an_aborted_write_settles() {
    let root = common::create_temp_dir();
    let env = Arc::new(BlockingWriteEnv {
        inner: NodeExecutionEnv::new(NodeExecutionEnvOptions {
            cwd: root.to_string(),
            ..Default::default()
        }),
        first_content: "first\n".to_string(),
        first_started: tokio::sync::watch::Sender::new(false),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        second_content: "second\n".to_string(),
        second_started: AtomicBool::new(false),
        writes: Mutex::new(Vec::new()),
    });
    let context = blocking_context(Arc::clone(&env));
    let tool = create_write_tool();
    let signal = CancellationToken::new();

    let first = {
        let tool = tool.clone();
        let context = context.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            run_with_signal(
                &tool,
                "write-first",
                json!({ "path": "file.txt", "content": "first\n" }),
                Some(&signal),
                &context,
            )
            .await
        })
    };
    wait_first_started(&env).await;
    signal.cancel();
    let second = {
        let tool = tool.clone();
        let context = context.clone();
        tokio::spawn(async move {
            run_with_signal(
                &tool,
                "write-second",
                json!({ "path": "file.txt", "content": "second\n" }),
                None,
                &context,
            )
            .await
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !env.second_started.load(Ordering::SeqCst),
        "the second write must wait for the aborted first write to settle"
    );
    env.release.add_permits(1);

    let first = first
        .await
        .expect("first task joins")
        .expect_err("the aborted write rejects");
    assert!(
        first.contains("aborted") || first.contains("Operation aborted"),
        "unexpected abort error: {first}"
    );
    let second = second.await.expect("second task").unwrap();
    let _ = second;
    let content = env_of(&context)
        .read_text_file("file.txt", None)
        .await
        .expect("read file");
    assert_eq!(content, "second\n");
}

/// Port of "keeps the mutation queue locked until an aborted edit write
/// settles".
#[tokio::test]
async fn keeps_the_mutation_queue_locked_until_an_aborted_edit_write_settles() {
    let root = common::create_temp_dir();
    let env = Arc::new(BlockingWriteEnv {
        inner: NodeExecutionEnv::new(NodeExecutionEnvOptions {
            cwd: root.to_string(),
            ..Default::default()
        }),
        first_content: "ALPHA\nbeta\n".to_string(),
        first_started: tokio::sync::watch::Sender::new(false),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        second_content: "ALPHA\nBETA\n".to_string(),
        second_started: AtomicBool::new(false),
        writes: Mutex::new(Vec::new()),
    });
    let context = blocking_context(Arc::clone(&env));
    write_file(env_of(&context), "file.txt", "alpha\nbeta\n").await;
    let tool = create_edit_tool();
    let signal = CancellationToken::new();

    let first = {
        let tool = tool.clone();
        let context = context.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            run_with_signal(
                &tool,
                "edit-first",
                json!({ "path": "file.txt", "edits": [{"oldText": "alpha", "newText": "ALPHA"}] }),
                Some(&signal),
                &context,
            )
            .await
        })
    };
    wait_first_started(&env).await;
    signal.cancel();
    let second = {
        let tool = tool.clone();
        let context = context.clone();
        tokio::spawn(async move {
            run_with_signal(
                &tool,
                "edit-second",
                json!({ "path": "file.txt", "edits": [{"oldText": "beta", "newText": "BETA"}] }),
                None,
                &context,
            )
            .await
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !env.second_started.load(Ordering::SeqCst),
        "the second edit must wait for the aborted first edit to settle; writes so far: {:?}",
        env.writes.lock().unwrap().clone()
    );
    env.release.add_permits(1);

    let first = first
        .await
        .expect("first task joins")
        .expect_err("the aborted edit rejects");
    assert!(first.contains("aborted"), "unexpected abort error: {first}");
    let second = second.await.expect("second task").unwrap();
    let _ = second;
    let content = env_of(&context)
        .read_text_file("file.txt", None)
        .await
        .expect("read file");
    assert_eq!(content, "ALPHA\nBETA\n");
}

/// Port of "edits regular files through symlinks".
#[tokio::test]
async fn edits_regular_files_through_symlinks() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    write_file(env_of(&context), "target.txt", "before\n").await;
    #[cfg(unix)]
    std::os::unix::fs::symlink("target.txt", format!("{root}/link.txt")).expect("symlink");

    let result = run(
        &create_edit_tool(),
        json!({ "path": "link.txt", "edits": [{"oldText": "before", "newText": "after"}] }),
        &context,
    )
    .await;

    // The TS case edits through the symlink; on unix the symlink resolves.
    #[cfg(unix)]
    {
        let result = result.expect("edit through symlink");
        let _ = result;
        let content = env_of(&context)
            .read_text_file("target.txt", None)
            .await
            .expect("read target");
        assert_eq!(content, "after\n");
    }
    #[cfg(not(unix))]
    {
        let _ = result;
    }
}

/// Port of "coalesces updates and persists truncated full output".
#[tokio::test]
async fn bash_coalesces_updates_and_persists_truncated_full_output() {
    let root = common::create_temp_dir();
    let context = context_for(&root);
    let updates: Arc<Mutex<Vec<AgentToolResult>>> = Arc::new(Mutex::new(Vec::new()));
    let callback: pi_core::agent::types::AgentToolUpdateCallback = {
        let updates = Arc::clone(&updates);
        Arc::new(move |update: AgentToolResult| {
            updates.lock().unwrap().push(update);
        })
    };

    let result = ((create_bash_tool(BashToolOptions::default()).execute)(
        "bash-5",
        &json!({ "command": "i=1; while [ $i -le 3000 ]; do echo line-$i; i=$((i + 1)); done" }),
        None,
        Some(&callback),
        &context,
    ))
    .await
    .expect("bash run");

    let updates = updates.lock().unwrap().clone();
    assert!(
        updates.len() < 25,
        "expected coalescing, got {} updates",
        updates.len()
    );
    let details = result.details.clone();
    let truncation = details
        .pointer("/truncation")
        .cloned()
        .expect("truncation details");
    assert_eq!(truncation.pointer("/truncated"), Some(&json!(true)));
    assert_eq!(truncation.pointer("/truncatedBy"), Some(&json!("lines")));
    assert_eq!(truncation.pointer("/totalLines"), Some(&json!(3000)));
    assert_eq!(truncation.pointer("/outputLines"), Some(&json!(2000)));
    assert!(text_output(&result).contains("line-3000"));
    let full_output_path = details
        .pointer("/fullOutputPath")
        .and_then(|value| value.as_str())
        .expect("fullOutputPath")
        .to_string();
    let last = updates.last().expect("final update");
    assert!(text_output(last).contains("line-3000"));
    assert_eq!(
        last.details.pointer("/truncation/totalLines").cloned(),
        Some(json!(3000))
    );
    assert_eq!(
        last.details.pointer("/fullOutputPath").cloned(),
        Some(json!(full_output_path))
    );
    let full_output = env_of(&context)
        .read_text_file(
            full_output_path
                .strip_prefix(&format!("{root}/"))
                .unwrap_or(full_output_path.as_str()),
            None,
        )
        .await
        .or_else(|_| std::fs::read_to_string(&full_output_path).map_err(|error| error.to_string()))
        .expect("read full output");
    assert!(full_output.contains("line-1\nline-2"));
    assert!(full_output.contains("line-2999\nline-3000"));
}
