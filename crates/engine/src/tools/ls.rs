//! Directory listing for the agent-facing `ls` API — the model's only way
//! to see a directory's structure without a shell (ADR-0025's read-only
//! planning surface has no `bash`). One `read_dir` per call is a small
//! blocking call, inline like [`super`]'s `LocalExecutionEnv` methods; the
//! entry cap only keeps huge directories (node_modules) from flooding the
//! transcript.

use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Entries returned before the listing stops with a truncation notice.
const MAX_ENTRIES: usize = 500;

const DESCRIPTION: &str = "List one directory's entries — subdirectories carry a \
trailing `/`, hidden entries are included. Directories-first, then files, both \
alphabetical. This is how to discover a project's structure: `read` reads files, \
never directories. Output stops at 500 entries with a counted notice; list a more \
specific subdirectory instead of re-listing.";

#[derive(Debug, Deserialize)]
struct LsInput {
    #[serde(default)]
    path: Option<String>,
}

/// One directory listing: sorted entry lines plus the cap state.
struct Listing {
    lines: Vec<String>,
    total: usize,
    truncated: bool,
}

impl Listing {
    fn into_result(self, path: &str) -> AgentToolResult {
        let text = if self.lines.is_empty() {
            "(empty directory)".to_string()
        } else {
            let mut text = self.lines.join("\n");
            if self.truncated {
                let hidden = self.total - self.lines.len();
                text.push_str(&format!(
                    "\n[truncated at {} entries: {hidden} more — list a more specific \
subdirectory]",
                    self.lines.len()
                ));
            }
            text
        };
        AgentToolResult {
            content: vec![BlockContent::Text(TextContent {
                text,
                ..Default::default()
            })],
            details: json!({
                "path": path,
                "entries": self.total,
                "truncated": self.truncated,
            }),
            ..Default::default()
        }
    }
}

/// The listing body — one `read_dir`, inline (small blocking call). Entries
/// classify through a target-following `is_dir`, so a symlinked directory
/// lists as `name/`; a broken symlink lists as a plain name.
fn run_list(dir: &PathBuf) -> Result<Listing, String> {
    let reader =
        std::fs::read_dir(dir).map_err(|error| format!("directory cannot be listed: {error}"))?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut total = 0usize;
    for entry in reader {
        let entry = entry.map_err(|error| format!("directory cannot be listed: {error}"))?;
        total += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.path().is_dir();
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort_by_key(|name| name.to_lowercase());
    files.sort_by_key(|name| name.to_lowercase());
    let mut lines = dirs;
    lines.extend(files);
    let truncated = lines.len() > MAX_ENTRIES;
    lines.truncate(MAX_ENTRIES);
    Ok(Listing {
        lines,
        total,
        truncated,
    })
}

fn parse_spec(cwd: &str, params: &serde_json::Value) -> Result<PathBuf, String> {
    let input = serde_json::from_value::<LsInput>(params.clone())
        .map_err(|error| format!("invalid ls parameters: {error}"))?;
    // Same path semantics as the other tools: relative to the chat's cwd,
    // absolute paths pass through, no confinement.
    let dir = PathBuf::from(super::to_absolute(
        cwd,
        input.path.as_deref().unwrap_or("."),
    ));
    if !dir.is_dir() {
        // Name which case failed: a missing path and a file path are
        // different model mistakes.
        if dir.exists() {
            return Err(format!("not a directory: {}", dir.display()));
        }
        return Err(format!("path does not exist: {}", dir.display()));
    }
    Ok(dir)
}

pub(crate) fn create_ls_tool(cwd: &str) -> AgentTool {
    let tool_cwd = cwd.to_owned();
    let execute = Arc::new(
        move |_tool_call_id: &str,
              params: &serde_json::Value,
              _signal: Option<&CancellationToken>,
              _on_update: Option<&AgentToolUpdateCallback>| {
            let spec = parse_spec(&tool_cwd, params);
            Box::pin(async move {
                let dir = spec?;
                let display = dir.display().to_string();
                run_list(&dir).map(|listing| listing.into_result(&display))
            }) as BoxFuture<'static, Result<AgentToolResult, String>>
        },
    );
    AgentTool {
        name: "ls".to_string(),
        label: "Ls".to_string(),
        description: DESCRIPTION.to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list; relative paths resolve \
        against the working directory. Default: the working directory"
                }
            }
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (dir.path().to_path_buf(), dir)
    }

    fn write(root: &std::path::Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn lists_dirs_first_with_trailing_slash_then_files() {
        let (root, _guard) = temp_root();
        write(&root, "zebra.txt", "x");
        write(&root, "src/main.rs", "x");
        write(&root, ".hidden", "x");
        let listing = run_list(&root).unwrap();
        assert_eq!(
            listing.lines,
            vec![
                "src/".to_string(),
                ".hidden".to_string(),
                "zebra.txt".to_string()
            ]
        );
        assert_eq!(listing.total, 3);
        assert!(!listing.truncated);
    }

    #[test]
    fn sorting_within_each_group_is_case_insensitive() {
        let (root, _guard) = temp_root();
        write(&root, "beta", "x");
        write(&root, "Alpha", "x");
        let listing = run_list(&root).unwrap();
        assert_eq!(listing.lines, vec!["Alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn cap_truncates_with_a_counted_notice() {
        let (root, _guard) = temp_root();
        for i in 0..(MAX_ENTRIES + 10) {
            write(&root, &format!("f{i:04}.txt"), "x");
        }
        let listing = run_list(&root).unwrap();
        assert!(listing.truncated);
        assert_eq!(listing.lines.len(), MAX_ENTRIES);
        assert_eq!(listing.total, MAX_ENTRIES + 10);
        let result = listing.into_result("root");
        let text = match result.content.first().unwrap() {
            BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(
            text.contains("[truncated at 500 entries: 10 more"),
            "text: {text}"
        );
        assert_eq!(result.details["truncated"], json!(true));
    }

    #[test]
    fn empty_directory_names_itself() {
        let (root, _guard) = temp_root();
        let listing = run_list(&root).unwrap();
        let result = listing.into_result("root");
        let text = match result.content.first().unwrap() {
            BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert_eq!(text, "(empty directory)");
    }

    #[test]
    fn parse_spec_rejects_missing_and_file_paths() {
        let (root, _guard) = temp_root();
        let cwd = root.to_str().unwrap();
        let missing = parse_spec(cwd, &json!({ "path": "nope" })).unwrap_err();
        assert!(missing.contains("does not exist"), "unexpected: {missing}");
        write(&root, "f.txt", "x");
        let file = parse_spec(cwd, &json!({ "path": "f.txt" })).unwrap_err();
        assert!(file.contains("not a directory"), "unexpected: {file}");
        let default_ok = parse_spec(cwd, &json!({})).unwrap();
        assert_eq!(default_ok, root);
    }

    #[tokio::test]
    async fn tool_executes_end_to_end() {
        let (root, _guard) = temp_root();
        write(&root, "sub/inner.rs", "x");
        write(&root, "top.txt", "x");
        let tool = create_ls_tool(root.to_str().unwrap());
        let result = (tool.execute)("call-1", &json!({ "path": "sub" }), None, None)
            .await
            .unwrap();
        let text = match result.content.first().unwrap() {
            BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert_eq!(text, "inner.rs");
        assert_eq!(result.details["entries"], json!(1));
    }

    #[test]
    fn execution_tools_mounts_ls() {
        let (root, _guard) = temp_root();
        let tools = crate::tools::execution_tools(root.to_str().unwrap());
        assert_eq!(tools.len(), 7);
        assert!(tools.iter().any(|tool| tool.name == "ls"));
    }
}
