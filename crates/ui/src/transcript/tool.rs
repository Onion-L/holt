//! Tool-chip detail payloads and sidecar logic: detail construction
//! (output/diff/stats), full-invocation blocks, analytic chip/detail heights,
//! group summaries, and the blob-fetch upgrade helpers. Pure values — no
//! GPUI elements.

use std::sync::Arc;

use gpui::SharedString;
use holt_proto::ToolCall;

use super::ToolItem;
use crate::markdown::parser::InlineRun;
/// Tool chip row height / gap — analytic, so fold heights need no measurement.
/// A row is a FLAT quiet line (no card chrome): the header row is the whole
/// chip, so one constant is both the row and its header. Rows stack with no
/// gap so the guide rail reads continuous.
pub const CHIP_HEIGHT: f32 = 28.0;
pub const CHIP_GAP: f32 = 0.0;

pub(super) const CHIPS_TOP_PAD: f32 = 2.0;

/// A chip's expandable detail payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDetail {
    /// Command/tool output as a code block: verbatim lines (indentation
    /// intact), capped at [`OUTPUT_DETAIL_MAX_LINES`] with a counted tail.
    Output {
        lines: Vec<SharedString>,
        truncated_by: usize,
    },
    /// A thought's markdown, pre-flattened into wrapped STYLED lines — one
    /// fixed-height row each, so the height stays analytic like `Output`
    /// while inline markers render as real styling ([`thought_lines`]).
    Thought {
        lines: Vec<Vec<InlineRun>>,
        truncated_by: usize,
    },
    /// A file diff, in the changes pane's model: hunks with 3 lines of
    /// context, dual line numbers, and (for recognized languages) syntax
    /// tokens — rendered by `changes::render_file_body`.
    Diff {
        file: Arc<crate::changes::FileDiff>,
        old_text: Option<Arc<str>>,
        new_text: Option<Arc<str>>,
    },
    /// Per-file `+N −N` stat rows — what the thin doc keeps of an edit
    /// (chat2-sync A1). The full diff upgrades this to [`ToolDetail::Diff`]
    /// via the sidecar fetch.
    Stats {
        stats: Arc<Vec<holt_doc::ToolDiffStat>>,
    },
}

/// Max verbatim lines per THOUGHT detail or invocation block before the
/// counted tail row. Tool OUTPUT details ride higher — [`FULL_OUTPUT_MAX_LINES`]
/// — since the doc carries the full content now.
pub const OUTPUT_DETAIL_MAX_LINES: usize = 24;

/// Max diff lines an inline tool-diff detail renders — the detail is one
/// stacked element inside its transcript row, so it must stay bounded
/// (~600 lines ≈ 12.6k px, several screens of context before the cut).
pub const DIFF_DETAIL_MAX_LINES: usize = 600;

/// Per-line height of an output detail block (diff blocks use the changes
/// pane's own [`crate::changes::DIFF_LINE_HEIGHT`]).
pub const OUTPUT_LINE_HEIGHT: f32 = 18.0;

/// Vertical padding of an output detail body (py(6) × 2).
const OUTPUT_BODY_PAD: f32 = 12.0;

/// Build a tool part's expandable detail. A diff wins over raw output (it is
/// the more structured record of the same action); post-strip docs carry diff
/// STATS instead of inline diff text, which win the same way.
pub fn tool_detail(
    output: Option<&str>,
    diff: Option<&holt_proto::ToolDiff>,
    diff_stats: Option<&[holt_doc::ToolDiffStat]>,
) -> Option<ToolDetail> {
    if let Some(diff) = diff {
        let mut file = diff_to_file(diff);
        if file.hunks.is_empty() {
            return None;
        }
        // A transcript diff renders as one stacked element inside its row —
        // cap it so a whole-file rewrite (or fetched full-diff blob) can't
        // build tens of thousands of elements per frame. The changes pane
        // has no such cap; it virtualizes per line.
        crate::changes::truncate_file_lines(&mut file, DIFF_DETAIL_MAX_LINES);
        return Some(ToolDetail::Diff {
            file: Arc::new(file),
            old_text: diff.old_text.as_deref().map(Arc::from),
            new_text: Some(Arc::from(diff.new_text.as_str())),
        });
    }
    if let Some(stats) = diff_stats.filter(|s| !s.is_empty()) {
        return Some(ToolDetail::Stats {
            stats: Arc::new(stats.to_vec()),
        });
    }
    let output = output?;
    let mut lines: Vec<SharedString> = output
        .lines()
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    // Trim trailing blank output lines so the block hugs its content.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    // The doc carries the FULL tool output (the engine no longer summarizes),
    // so the inline cap matches the fetched-blob ceiling — a Read shows its
    // whole file up to pi-core's own truncation point.
    let truncated_by = lines.len().saturating_sub(FULL_OUTPUT_MAX_LINES);
    lines.truncate(FULL_OUTPUT_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Columns at which an invocation line soft-wraps into continuation lines.
/// The wrap is char-counted, not measured — block heights must be analytic —
/// so the budget is sized to fit the narrowest useful transcript pane.
pub const CALL_WRAP_COLS: usize = 80;

/// Soft-wrap one raw line into [`CALL_WRAP_COLS`]-char chunks so a long
/// single-line command stays fully readable instead of ellipsizing.
pub(super) fn wrap_cols(line: &str, cols: usize) -> Vec<SharedString> {
    if line.chars().count() <= cols {
        return vec![SharedString::from(line.to_owned())];
    }
    line.chars()
        .collect::<Vec<_>>()
        .chunks(cols)
        .map(|chunk| SharedString::from(chunk.iter().collect::<String>()))
        .collect()
}

/// Build a chip's full-invocation block — the complete tool call the header
/// truncates to one line: the whole command, pattern, or URL, todo items one
/// per line, MCP/unknown input as pretty-printed JSON. Reuses the output
/// code-block payload so rendering and height stay one implementation.
pub fn call_block(call: &ToolCall) -> Option<ToolDetail> {
    let text: String = match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::ReadFile { path } => path.clone(),
        ToolCall::WriteFile { path, content } => match content {
            Some(content) => format!("{path}\n{content}"),
            None => path.clone(),
        },
        ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_else(|| "workspace".into()),
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.clone(),
        },
        ToolCall::Glob { pattern } => pattern.clone(),
        ToolCall::WebFetch { url, prompt } => match prompt {
            Some(prompt) => format!("{url}\n{prompt}"),
            None => url.clone(),
        },
        ToolCall::WebSearch { query } => query.clone(),
        ToolCall::Todo { items } => items
            .iter()
            .map(|i| format!("{} {}", if i.done { "[x]" } else { "[ ]" }, i.text))
            .collect::<Vec<_>>()
            .join("\n"),
        ToolCall::Mcp {
            server,
            tool,
            input,
        } => match input {
            Some(input) => format!(
                "{server} · {tool}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => format!("{server} · {tool}"),
        },
        ToolCall::Unknown { name, input } => match input {
            Some(input) => format!(
                "{name}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => name.clone(),
        },
    };
    let mut lines: Vec<SharedString> = text
        .lines()
        .flat_map(|l| wrap_cols(l, CALL_WRAP_COLS))
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Reduce an inline [`holt_proto::ToolDiff`] to the changes pane's
/// [`crate::changes::FileDiff`]: hunks grouped with 3 context lines, dual
/// 1-based line numbers, unified-diff hunk headers, and add/del counts.
pub fn diff_to_file(diff: &holt_proto::ToolDiff) -> crate::changes::FileDiff {
    use crate::changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
    let old = diff.old_text.as_deref().unwrap_or("");
    let text_diff = similar::TextDiff::from_lines(old, &diff.new_text);
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    let mut max_line = 0u32;
    for group in text_diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_range.start + 1,
            old_range.len(),
            new_range.start + 1,
            new_range.len(),
        );
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        LineKind::Del
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Equal => LineKind::Context,
                };
                let old_no = change.old_index().map(|n| n as u32 + 1);
                let new_no = change.new_index().map(|n| n as u32 + 1);
                max_line = max_line.max(old_no.unwrap_or(0)).max(new_no.unwrap_or(0));
                lines.push(DiffLine {
                    kind,
                    old_no,
                    new_no,
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
        hunks.push(Hunk { header, lines });
    }
    FileDiff {
        path: diff.path.clone(),
        old_path: None,
        status: if diff.old_text.is_none() {
            FileStatus::Added
        } else {
            FileStatus::Modified
        },
        binary: false,
        notices: Vec::new(),
        hunks,
        additions,
        deletions,
        max_line,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries / chips (pure)
// ---------------------------------------------------------------------------

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// The rule lives in `holt_proto::view` so the terminal viewport reports the
/// same summary; this only adapts the row model's [`ToolItem`] to it.
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools
        .iter()
        .filter(|t| !t.is_thought)
        .map(|t| (t.call.clone(), t.is_error))
        .collect();
    let thoughts = tools.iter().filter(|t| t.is_thought).count();
    // The shared summary answers "used 0 tools" for an empty set — a
    // thought-only group must not inherit that.
    let base = if pairs.is_empty() {
        String::new()
    } else {
        holt_proto::view::tool_group_summary(&pairs)
    };
    // Thought chips ride the group (they are UI-synthesized, so the shared
    // view summary never sees them): name them on the collapsed line.
    match (base.is_empty(), thoughts) {
        (_, 0) => base,
        (true, 1) => "Thought process".into(),
        (true, n) => format!("Thought {n} times"),
        (false, 1) => format!("Thought · {base}"),
        (false, n) => format!("Thought {n} times · {base}"),
    }
}

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

/// Analytic height an open detail adds to its chip's row — output blocks by
/// line count, diff blocks via the changes pane's own
/// [`crate::changes::body_height`]. The chip's own [`CHIP_HEIGHT`] is already
/// counted by [`chips_height`].
pub fn detail_height(detail: &ToolDetail) -> f32 {
    match detail {
        ToolDetail::Output {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Diff { file, .. } => crate::changes::body_height(file),
        ToolDetail::Stats { stats } => stats.len() as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD,
    }
}

/// Height of the "Show full output/diff" affordance row appended below an
/// open detail whose full payload lives in the sidecar (chat2-sync A3).
pub const BLOB_AFFORDANCE_HEIGHT: f32 = 24.0;

/// What an open chip's [`BLOB_AFFORDANCE_HEIGHT`] row offers: a lazy sidecar
/// fetch ("Show full output/diff"). One slot, so the analytic height sums
/// stay a single `is_some` check.
#[derive(Clone)]
pub(super) struct ChipAffordance {
    pub(super) blob_ref: SharedString,
    pub(super) label: SharedString,
}

/// Line cap for a FETCHED full output (a defensive ceiling, not a doc cap —
/// the provider bounds outputs at 4KiB, so this is rarely reached).
pub(super) const FULL_OUTPUT_MAX_LINES: usize = 400;

/// Build the upgraded detail from a fetched sidecar blob. Diff blobs parse
/// the `ToolDiff` JSON through the same pipeline as inline diffs; output
/// blobs render (near-)uncapped — fetching past the summary was the point.
pub(super) fn blob_detail(text: &str, is_diff: bool) -> Option<ToolDetail> {
    if is_diff {
        let diff: holt_proto::ToolDiff = serde_json::from_str(text).ok()?;
        return tool_detail(None, Some(&diff), None);
    }
    let mut lines: Vec<SharedString> = text
        .lines()
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(FULL_OUTPUT_MAX_LINES);
    lines.truncate(FULL_OUTPUT_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Compact byte size for the fetch affordance label ("812 B", "12 KB").
pub(super) fn format_kb(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_diff_builds_real_hunks_with_context_and_numbers() {
        use crate::changes::LineKind;
        let old = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let mut new = old.clone();
        new[9] = "LINE 10".into();
        let diff = holt_proto::ToolDiff {
            path: "/w/a.rs".into(),
            old_text: Some(old.join("\n") + "\n"),
            new_text: new.join("\n") + "\n",
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&diff), None)
        else {
            panic!("expected diff detail");
        };
        // One hunk: the change plus 3 context lines each side, real numbers.
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -7,7 +7,7 @@");
        assert_eq!(hunk.lines.len(), 8); // 6 context + 1 del + 1 add
        let del = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Del)
            .expect("del line");
        assert_eq!(del.old_no, Some(10));
        assert_eq!(del.new_no, None);
        assert_eq!(del.text, "line 10");
        let add = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Add)
            .expect("add line");
        assert_eq!(add.new_no, Some(10));
        assert_eq!(add.text, "LINE 10");
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(old_text.as_deref(), diff.old_text.as_deref());
        assert_eq!(new_text.as_deref(), Some(diff.new_text.as_str()));
        // New files carry Added status (and no old numbers).
        let created = holt_proto::ToolDiff {
            path: "/w/new.txt".into(),
            old_text: None,
            new_text: "only\n".into(),
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&created), None)
        else {
            panic!("expected diff detail");
        };
        assert_eq!(file.status, crate::changes::FileStatus::Added);
        assert!(old_text.is_none());
        assert_eq!(new_text.as_deref(), Some("only\n"));

        // Output: verbatim lines (indentation intact), counted-tail cap.
        // 40 lines rides whole — the doc carries full output now, the inline
        // cap is the fetched-blob ceiling (400), not the old 24.
        let output = (0..40)
            .map(|i| format!("    indented {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = tool_detail(Some(&output), None, None)
        else {
            panic!("expected output detail");
        };
        assert_eq!(lines.len(), 40);
        assert_eq!(truncated_by, 0);
        assert_eq!(lines[0].as_ref(), "    indented 0");

        // Past the ceiling the counted tail returns.
        let big = (0..FULL_OUTPUT_MAX_LINES + 9)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = tool_detail(Some(&big), None, None)
        else {
            panic!("expected output detail");
        };
        assert_eq!(lines.len(), FULL_OUTPUT_MAX_LINES);
        assert_eq!(truncated_by, 9);

        // Nothing → no affordance.
        assert!(tool_detail(None, None, None).is_none());
        assert!(tool_detail(Some("\n\n"), None, None).is_none());
    }

    #[test]
    fn tool_group_summaries() {
        let exec = |c: &str| ToolItem {
            call: ToolCall::Exec { command: c.into() },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
            gate: None,
        };
        let edit = |p: &str| ToolItem {
            call: ToolCall::EditFile {
                path: p.into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
            gate: None,
        };
        let tools = vec![
            exec("ls"),
            exec("pwd"),
            exec("make"),
            edit("a.rs"),
            edit("b.rs"),
        ];
        assert_eq!(
            tool_group_summary(&tools),
            "Ran 3 commands · edited 2 files"
        );
        // Distinct-path dedupe: editing one file twice counts once.
        let tools = vec![edit("a.rs"), edit("a.rs")];
        assert_eq!(tool_group_summary(&tools), "Edited 1 file");
        // Failures append.
        let mut failing = exec("boom");
        failing.is_error = true;
        assert_eq!(tool_group_summary(&[failing]), "Ran 1 command · 1 failed");
        // Reads / searches / misc.
        let tools = vec![
            ToolItem {
                call: ToolCall::ReadFile { path: "x".into() },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
                gate: None,
            },
            ToolItem {
                call: ToolCall::Glob {
                    pattern: "*.rs".into(),
                },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
                gate: None,
            },
            ToolItem {
                call: ToolCall::WebSearch { query: "q".into() },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
                gate: None,
            },
        ];
        assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
    }

    #[test]
    fn call_block_carries_the_full_invocation() {
        // Multi-line command: verbatim lines, not the flattened chip line.
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = call_block(&ToolCall::Exec {
            command: "set -e\ncargo test".into(),
        })
        else {
            panic!("expected an output block")
        };
        assert_eq!(truncated_by, 0);
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["set -e", "cargo test"]
        );

        // A long single-line command soft-wraps instead of ellipsizing.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Exec {
            command: "x".repeat(CALL_WRAP_COLS * 2 + 10),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.chars().count() <= CALL_WRAP_COLS));

        // MCP input pretty-prints under the `server · tool` line.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Mcp {
            server: "gh".into(),
            tool: "issues".into(),
            input: Some(serde_json::json!({"repo": "holt"})),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines[0].as_ref(), "gh · issues");
        assert!(lines.iter().any(|l| l.contains("\"repo\": \"holt\"")));

        // Todos list one item per line with checkbox state.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Todo {
            items: vec![
                holt_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                holt_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["[x] a", "[ ] b"]
        );

        // Blank invocation → no block; the chip stays a plain card.
        assert!(
            call_block(&ToolCall::Exec {
                command: "  \n ".into()
            })
            .is_none()
        );
    }

    #[test]
    fn chips_height_is_analytic() {
        assert_eq!(chips_height(0), 0.0);
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        assert_eq!(
            chips_height(3),
            CHIPS_TOP_PAD + 3.0 * CHIP_HEIGHT + 2.0 * CHIP_GAP
        );
    }
}
