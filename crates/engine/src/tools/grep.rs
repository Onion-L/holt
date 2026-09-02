//! Content search for the agent-facing `grep` API, built on ripgrep's own
//! crates in process (ADR-0004). Unlike the env-backed
//! built-ins — small blocking calls by design, see [`super`]'s `LocalExecutionEnv`] —
//! a search can walk a whole tree, so the walk runs on `spawn_blocking` and
//! checks the run's cancellation token between files.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::BoxFuture;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Byte envelope for the text handed back to the model.
const OUTPUT_BYTE_CAP: usize = 50 * 1024;
/// Per-line truncation before formatting (chars, not bytes).
const LINE_CHAR_CAP: usize = 300;
const DEFAULT_HEAD_LIMIT: usize = 100;
const MAX_HEAD_LIMIT: usize = 1000;
const MAX_CONTEXT: usize = 50;

const DESCRIPTION: &str = "Search file contents with a regex and return matches as \
`path:line:content` lines (paths relative to the working directory; context lines use \
`path-line-content`). Respects .gitignore inside git repositories, searches hidden files, \
skips binary files and the .git directory. Output stops early at head_limit (default 100 \
match lines) or a 50KB envelope and reports truncation; when results overflow, prefer \
output_mode=files_with_matches, a glob filter, or a narrower path.";

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    before_context: Option<usize>,
    #[serde(default)]
    after_context: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn as_str(self) -> &'static str {
        match self {
            OutputMode::Content => "content",
            OutputMode::FilesWithMatches => "files_with_matches",
            OutputMode::Count => "count",
        }
    }
}

#[derive(Debug)]
struct SearchSpec {
    pattern: String,
    cwd: PathBuf,
    root: PathBuf,
    glob: Option<String>,
    mode: OutputMode,
    case_insensitive: bool,
    head_limit: usize,
    before_context: usize,
    after_context: usize,
}

fn parse_spec(cwd: &str, params: &serde_json::Value) -> Result<SearchSpec, String> {
    let input = serde_json::from_value::<GrepInput>(params.clone())
        .map_err(|error| format!("invalid grep parameters: {error}"))?;
    let mode = match input.output_mode.as_deref() {
        None | Some("content") => OutputMode::Content,
        Some("files_with_matches") => OutputMode::FilesWithMatches,
        Some("count") => OutputMode::Count,
        Some(other) => {
            return Err(format!(
                "unknown output_mode {other:?}; expected content, files_with_matches, or count"
            ));
        }
    };
    // Same path semantics as the other tools: relative to the chat's cwd,
    // absolute paths pass through, no confinement.
    let root = super::to_absolute(cwd, input.path.as_deref().unwrap_or("."));
    if !Path::new(&root).exists() {
        return Err(format!("search path does not exist: {root}"));
    }
    let clamp_context = |value: usize| value.min(MAX_CONTEXT);
    let before_context = clamp_context(
        input
            .context
            .unwrap_or(0)
            .max(input.before_context.unwrap_or(0)),
    );
    let after_context = clamp_context(
        input
            .context
            .unwrap_or(0)
            .max(input.after_context.unwrap_or(0)),
    );
    Ok(SearchSpec {
        pattern: input.pattern,
        cwd: PathBuf::from(cwd),
        root: PathBuf::from(root),
        glob: input.glob,
        mode,
        case_insensitive: input.case_insensitive,
        head_limit: input
            .head_limit
            .unwrap_or(DEFAULT_HEAD_LIMIT)
            .clamp(1, MAX_HEAD_LIMIT),
        before_context,
        after_context,
    })
}

/// Paths display relative to the chat's cwd when they live under it (fewer
/// tokens), absolute otherwise.
fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn truncate_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\n', '\r']);
    if text.chars().count() <= LINE_CHAR_CAP {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(LINE_CHAR_CAP).collect();
    out.push('…');
    out
}

struct Collector {
    cwd: PathBuf,
    mode: OutputMode,
    head_limit: usize,
    lines: Vec<String>,
    bytes: usize,
    /// Total match lines emitted (content mode).
    matches: usize,
    /// Total files listed (files_with_matches and count modes).
    files: usize,
    /// Match lines in the current file (count mode).
    file_count: usize,
    truncated: bool,
    warnings: usize,
    /// Set once a cap is hit; stops both the current file and the walk.
    done: bool,
    file_path: String,
}

impl Collector {
    fn new(cwd: PathBuf, mode: OutputMode, head_limit: usize) -> Self {
        Self {
            cwd,
            mode,
            head_limit,
            lines: Vec::new(),
            bytes: 0,
            matches: 0,
            files: 0,
            file_count: 0,
            truncated: false,
            warnings: 0,
            done: false,
            file_path: String::new(),
        }
    }

    fn begin_file(&mut self, path: &Path) {
        self.file_path = display_path(&self.cwd, path);
        self.file_count = 0;
    }

    /// Push a formatted line inside the byte envelope; false = envelope full.
    fn push_line(&mut self, line: String) -> bool {
        if self.bytes + line.len() + 1 > OUTPUT_BYTE_CAP {
            return false;
        }
        self.bytes += line.len() + 1;
        self.lines.push(line);
        true
    }

    fn overflow(&mut self) {
        self.truncated = true;
        self.done = true;
    }

    fn warn(&mut self) {
        self.warnings += 1;
    }

    fn on_match(&mut self, line_number: Option<u64>, bytes: &[u8]) -> bool {
        if self.done {
            return false;
        }
        match self.mode {
            OutputMode::Content => {
                if self.matches >= self.head_limit {
                    self.overflow();
                    return false;
                }
                let line = format!(
                    "{}:{}:{}",
                    self.file_path,
                    line_number.unwrap_or(0),
                    truncate_line(bytes)
                );
                if !self.push_line(line) {
                    self.overflow();
                    return false;
                }
                self.matches += 1;
                true
            }
            OutputMode::FilesWithMatches => {
                if self.files >= self.head_limit {
                    self.overflow();
                    return false;
                }
                let line = self.file_path.clone();
                if !self.push_line(line) {
                    self.overflow();
                    return false;
                }
                self.files += 1;
                // The file is on the list; the rest of it cannot add
                // information (rg -l semantics).
                false
            }
            OutputMode::Count => {
                self.file_count += 1;
                true
            }
        }
    }

    fn on_context(&mut self, line_number: Option<u64>, bytes: &[u8]) -> bool {
        if self.done || self.mode != OutputMode::Content {
            return false;
        }
        let line = format!(
            "{}-{}-{}",
            self.file_path,
            line_number.unwrap_or(0),
            truncate_line(bytes)
        );
        if !self.push_line(line) {
            self.overflow();
            return false;
        }
        true
    }

    fn end_file(&mut self) {
        if self.mode == OutputMode::Count && self.file_count > 0 && !self.done {
            if self.files >= self.head_limit {
                self.overflow();
            } else {
                let line = format!("{}:{}", self.file_path, self.file_count);
                if !self.push_line(line) {
                    self.overflow();
                } else {
                    self.files += 1;
                }
            }
        }
    }

    fn returned(&self) -> usize {
        match self.mode {
            OutputMode::Content => self.matches,
            _ => self.files,
        }
    }

    fn finish(mut self) -> SearchOutcome {
        let returned = self.returned();
        if self.truncated {
            // The walk stopped early, so the true total is unknown —
            // the notice can only promise "at least N".
            let unit = match self.mode {
                OutputMode::Content => "matches",
                _ => "files",
            };
            self.lines.push(format!(
                "[truncated after {returned} {unit}: more results exist — narrow the \
pattern, add a glob, or scope the path]"
            ));
        }
        if self.warnings > 0 {
            self.lines.push(format!(
                "[warning: search skipped {} unreadable entries or files]",
                self.warnings
            ));
        }
        SearchOutcome {
            lines: self.lines,
            truncated: self.truncated,
            returned,
            mode: self.mode,
            warnings: self.warnings,
        }
    }
}

#[derive(Debug)]
struct SearchOutcome {
    lines: Vec<String>,
    truncated: bool,
    returned: usize,
    mode: OutputMode,
    warnings: usize,
}

impl SearchOutcome {
    fn into_result(self) -> AgentToolResult {
        let text = if self.lines.is_empty() {
            "No matches found.".to_string()
        } else {
            self.lines.join("\n")
        };
        AgentToolResult {
            content: vec![BlockContent::Text(TextContent {
                text,
                ..Default::default()
            })],
            details: json!({
                "truncated": self.truncated,
                "returned": self.returned,
                "output_mode": self.mode.as_str(),
                "warnings": self.warnings,
            }),
            ..Default::default()
        }
    }
}

struct GrepSink<'a> {
    state: &'a mut Collector,
}

impl Sink for GrepSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        Ok(self.state.on_match(mat.line_number(), mat.bytes()))
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        Ok(self
            .state
            .on_context(context.line_number(), context.bytes()))
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        // A NUL byte marks the file binary and the rest of it is skipped;
        // the searcher also drops the read buffer that contained the NUL,
        // so small binary files contribute no match lines at all — rg
        // likewise reports no match text for them (only a stderr note).
        Ok(false)
    }
}

/// The search body — blocking, must run off the async runtime workers
/// (`spawn_blocking` in [`create_grep_tool`]). Unreadable entries and files
/// are skipped like rg's stderr-only warnings; a cancelled run stops the
/// walk between files.
fn run_search(spec: &SearchSpec, cancel: &CancellationToken) -> Result<SearchOutcome, String> {
    if cancel.is_cancelled() {
        return Err("search cancelled".to_string());
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(spec.case_insensitive)
        .build(&spec.pattern)
        .map_err(|error| format!("invalid regex: {error}"))?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(spec.before_context)
        .after_context(spec.after_context)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut walker = WalkBuilder::new(&spec.root);
    // The contract searches hidden files but never repository metadata:
    // with the hidden filter off the walker would descend into `.git`,
    // which rg skips only as a side effect of that filter.
    walker.hidden(false).filter_entry(|entry| {
        !(entry.file_type().is_some_and(|kind| kind.is_dir())
            && entry.file_name() == OsStr::new(".git"))
    });
    if let Some(glob) = spec.glob.as_deref() {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&spec.root);
        overrides
            .add(glob)
            .map_err(|error| format!("invalid glob {glob:?}: {error}"))?;
        walker.overrides(
            overrides
                .build()
                .map_err(|error| format!("invalid glob {glob:?}: {error}"))?,
        );
    }
    let mut state = Collector::new(spec.cwd.clone(), spec.mode, spec.head_limit);
    for entry in walker.build() {
        if cancel.is_cancelled() {
            return Err("search cancelled".to_string());
        }
        if state.done {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                state.warn();
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        state.begin_file(entry.path());
        let sink = GrepSink { state: &mut state };
        if searcher.search_path(&matcher, entry.path(), sink).is_err() {
            state.warn();
        }
        state.end_file();
    }
    Ok(state.finish())
}

fn parameters_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regex pattern (Rust regex syntax)"
            },
            "path": {
                "type": "string",
                "description": "Directory or file to search; relative paths \
    resolve against the working directory. Default: the working directory"
            },
            "glob": {
                "type": "string",
                "description": "File-name include filter, e.g. \"*.rs\" or \
    \"*.{ts,tsx}\"; prefix with ! to exclude"
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count"],
                "description": "content: `path:line:text` match lines; \
    files_with_matches: one path per line; count: `path:count` per file. \
    Default: content"
            },
            "case_insensitive": {
                "type": "boolean",
                "description": "Default: false"
            },
            "head_limit": {
                "type": "integer",
                "description": "Max match lines (content) or listed files \
    (other modes); the search stops early past this. Default: 100, max: 1000"
            },
            "context": {
                "type": "integer",
                "description": "Context lines shown before and after each \
    match (like -C)"
            },
            "before_context": {
                "type": "integer",
                "description": "Context lines before each match (like -B)"
            },
            "after_context": {
                "type": "integer",
                "description": "Context lines after each match (like -A)"
            }
        },
        "required": ["pattern"]
    })
}

pub(crate) fn create_grep_tool(cwd: &str) -> AgentTool {
    let tool_cwd = cwd.to_owned();
    let execute = Arc::new(
        move |_tool_call_id: &str,
              params: &serde_json::Value,
              signal: Option<&CancellationToken>,
              _on_update: Option<&AgentToolUpdateCallback>| {
            let spec = parse_spec(&tool_cwd, params);
            let cancel = signal.cloned().unwrap_or_default();
            Box::pin(async move {
                let spec = spec?;
                let cancel_for_task = cancel.clone();
                let search =
                    tokio::task::spawn_blocking(move || run_search(&spec, &cancel_for_task));
                tokio::select! {
                    _ = cancel.cancelled() => Err("search cancelled".to_string()),
                    joined = search => match joined {
                        Ok(Ok(outcome)) => Ok(outcome.into_result()),
                        Ok(Err(message)) => Err(message),
                        Err(error) => Err(format!("search task failed: {error}")),
                    },
                }
            }) as BoxFuture<'static, Result<AgentToolResult, String>>
        },
    );
    AgentTool {
        name: "grep".to_string(),
        label: "Grep".to_string(),
        description: DESCRIPTION.to_string(),
        parameters: parameters_schema(),
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

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn spec(root: &Path, pattern: &str) -> SearchSpec {
        SearchSpec {
            pattern: pattern.to_string(),
            cwd: root.to_path_buf(),
            root: root.to_path_buf(),
            glob: None,
            mode: OutputMode::Content,
            case_insensitive: false,
            head_limit: DEFAULT_HEAD_LIMIT,
            before_context: 0,
            after_context: 0,
        }
    }

    fn search(root: &Path, pattern: &str) -> SearchOutcome {
        run_search(&spec(root, pattern), &CancellationToken::new()).unwrap()
    }

    #[test]
    fn content_matches_as_path_line_text() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "fn one()\nfn two()\n");
        write(&root, "b.txt", "fn two()\n");
        let outcome = search(&root, "fn two");
        assert_eq!(
            outcome.lines,
            vec![
                "a.rs:2:fn two()".to_string(),
                "b.txt:1:fn two()".to_string()
            ]
        );
        assert!(!outcome.truncated);
    }

    #[test]
    fn hidden_files_are_searched_but_git_is_not() {
        let (root, _guard) = temp_root();
        write(&root, ".hidden/h.txt", "needle\n");
        write(&root, ".git/config", "needle\n");
        let outcome = search(&root, "needle");
        assert_eq!(outcome.lines, vec![".hidden/h.txt:1:needle".to_string()]);
    }

    #[test]
    fn gitignored_files_are_skipped() {
        let (root, _guard) = temp_root();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(&root, ".gitignore", "skip.txt\n");
        write(&root, "skip.txt", "needle\n");
        write(&root, "keep.txt", "needle\n");
        let outcome = search(&root, "needle");
        assert_eq!(outcome.lines, vec!["keep.txt:1:needle".to_string()]);
    }

    #[test]
    fn head_limit_marks_truncation() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "needle\nneedle\nneedle\nneedle\nneedle\n");
        let mut s = spec(&root, "needle");
        s.head_limit = 2;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.returned, 2);
        assert!(outcome.truncated);
        assert_eq!(outcome.lines.len(), 3);
        assert!(outcome.lines[2].contains("truncated after 2 matches"));
        let details = outcome.into_result().details;
        assert_eq!(details["truncated"], serde_json::json!(true));
        assert_eq!(details["returned"], serde_json::json!(2));
    }

    #[test]
    fn long_lines_truncate_to_char_cap() {
        let (root, _guard) = temp_root();
        let long = format!("needle{}", "x".repeat(500));
        write(&root, "a.rs", &format!("{long}\n"));
        let outcome = search(&root, "needle");
        let expected_len = "a.rs:1:".len() + LINE_CHAR_CAP + 1; // + ellipsis
        assert_eq!(outcome.lines[0].chars().count(), expected_len);
        assert!(outcome.lines[0].ends_with('…'));
    }

    #[test]
    fn byte_cap_truncates() {
        let (root, _guard) = temp_root();
        // ~170 of these 310-char formatted lines fit in the 50KB envelope.
        let line = format!("needle{}", "x".repeat(320));
        write(&root, "a.rs", &format!("{line}\n").repeat(200));
        let mut s = spec(&root, "needle");
        s.head_limit = MAX_HEAD_LIMIT;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert!(outcome.truncated);
        assert!(outcome.returned < 200);
        assert!(outcome.lines.last().unwrap().contains("truncated"));
    }

    #[test]
    fn count_mode_reports_per_file_line_counts() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "needle\nneedle\nother\n");
        write(&root, "sub/b.rs", "needle\nplain\n");
        let mut s = spec(&root, "needle");
        s.mode = OutputMode::Count;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        let mut lines = outcome.lines.clone();
        lines.sort();
        assert_eq!(lines, vec!["a.rs:2".to_string(), "sub/b.rs:1".to_string()]);
        assert_eq!(outcome.returned, 2);
    }

    #[test]
    fn files_with_matches_lists_paths_and_caps() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "needle\n");
        write(&root, "b.rs", "needle\n");
        write(&root, "c.rs", "needle\n");
        let mut s = spec(&root, "needle");
        s.mode = OutputMode::FilesWithMatches;
        s.head_limit = 2;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.returned, 2);
        assert!(outcome.truncated);
        assert!(
            outcome
                .lines
                .last()
                .unwrap()
                .contains("truncated after 2 files")
        );
        assert!(
            outcome
                .lines
                .iter()
                .take(2)
                .all(|line| line.ends_with(".rs"))
        );
    }

    #[test]
    fn case_insensitive_flag_matches_opposite_case() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "NeEdLe\n");
        let mut s = spec(&root, "needle");
        s.case_insensitive = true;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.lines, vec!["a.rs:1:NeEdLe".to_string()]);
        let sensitive = search(&root, "needle");
        assert!(sensitive.lines.is_empty());
    }

    #[test]
    fn context_lines_use_dash_separator() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "one\ntwo\nneedle\nfour\nfive\n");
        let mut s = spec(&root, "needle");
        s.before_context = 1;
        s.after_context = 1;
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(
            outcome.lines,
            vec![
                "a.rs-2-two".to_string(),
                "a.rs:3:needle".to_string(),
                "a.rs-4-four".to_string(),
            ]
        );
    }

    #[test]
    fn glob_filters_files() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "needle\n");
        write(&root, "b.txt", "needle\n");
        let mut s = spec(&root, "needle");
        s.glob = Some("*.rs".to_string());
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.lines, vec!["a.rs:1:needle".to_string()]);
    }

    #[test]
    fn absolute_root_outside_cwd_keeps_absolute_paths() {
        let (root, _guard) = temp_root();
        let (outside, _guard2) = temp_root();
        write(&outside, "f.txt", "needle\n");
        let mut s = spec(&root, "needle");
        s.root = outside.join("");
        let outcome = run_search(&s, &CancellationToken::new()).unwrap();
        assert_eq!(
            outcome.lines,
            vec![format!("{}:1:needle", outside.join("f.txt").display())]
        );
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let (root, _guard) = temp_root();
        let error = run_search(&spec(&root, "(["), &CancellationToken::new()).unwrap_err();
        assert!(error.contains("invalid regex"), "unexpected: {error}");
    }

    #[test]
    fn parse_spec_rejects_bad_mode_and_missing_root() {
        let (root, _guard) = temp_root();
        let bad_mode = parse_spec(
            root.to_str().unwrap(),
            &json!({ "pattern": "x", "output_mode": "loud" }),
        )
        .unwrap_err();
        assert!(bad_mode.contains("output_mode"), "unexpected: {bad_mode}");
        let missing = parse_spec(
            root.to_str().unwrap(),
            &json!({ "pattern": "x", "path": "nope" }),
        )
        .unwrap_err();
        assert!(missing.contains("does not exist"), "unexpected: {missing}");
    }

    #[test]
    fn binary_files_contribute_no_matches() {
        let (root, _guard) = temp_root();
        // The searcher drops the whole first read buffer that contained the
        // NUL, so nothing from the binary file is reported — same observable
        // behavior as rg, which prints only a "binary file matches" note.
        write(&root, "a.bin", "before\nrest\x00needle\n");
        write(&root, "b.txt", "needle\n");
        let outcome = search(&root, "needle");
        assert_eq!(outcome.lines, vec!["b.txt:1:needle".to_string()]);
    }

    #[test]
    fn cancelled_search_returns_error() {
        let (root, _guard) = temp_root();
        write(&root, "a.rs", "needle\n");
        let token = CancellationToken::new();
        token.cancel();
        let error = run_search(&spec(&root, "needle"), &token).unwrap_err();
        assert_eq!(error, "search cancelled");
    }

    #[test]
    fn skipped_io_errors_are_reported() {
        let (root, _guard) = temp_root();
        let mut collector = Collector::new(root, OutputMode::Content, DEFAULT_HEAD_LIMIT);
        collector.warn();
        let outcome = collector.finish();
        assert_eq!(outcome.warnings, 1);
        assert_eq!(
            outcome.lines,
            vec!["[warning: search skipped 1 unreadable entries or files]".to_string()]
        );
        assert_eq!(outcome.into_result().details["warnings"], json!(1));
    }

    #[tokio::test]
    async fn tool_executes_end_to_end() {
        let (root, _guard) = temp_root();
        write(&root, "f.txt", "hello needle\n");
        let tool = create_grep_tool(root.to_str().unwrap());
        let result = (tool.execute)("call-1", &json!({ "pattern": "needle" }), None, None)
            .await
            .unwrap();
        let text = match result.content.first().unwrap() {
            BlockContent::Text(text) => text.text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert_eq!(text, "f.txt:1:hello needle");
        assert_eq!(result.details["output_mode"], json!("content"));
    }

    #[test]
    fn execution_tools_mounts_grep() {
        let (root, _guard) = temp_root();
        let tools = crate::tools::execution_tools(root.to_str().unwrap());
        assert_eq!(tools.len(), 5);
        assert!(tools.iter().any(|tool| tool.name == "grep"));
    }
}
