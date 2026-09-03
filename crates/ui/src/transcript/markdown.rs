//! The transcript's Markdown wiring: the streaming/completed parse pipeline
//! (`parse_for_row` and its outcome accounting) and the thought flattener that
//! turns a parsed thought into wrapped, styled detail lines.

use std::collections::HashMap;
use std::sync::Arc;

use crate::markdown::parser::{
    Block, BlockTree, IncrementalParser, InlineRun, InlineStyle, parse_full,
};

use super::tool::wrap_cols;

/// Column budget for soft-wrapping thought text into detail lines. The
/// detail body is preformatted (no element wrapping), so the wrap happens
/// here — conservative enough to fit the card at typical transcript widths.
const THOUGHT_WRAP_COLS: usize = 96;

/// Flatten a thought's parsed markdown into wrapped, STYLED detail lines —
/// inline markers render as real styling (bold/italic/code/links) instead of
/// literal `**` glyphs; blocks flatten structurally (headings bold, list
/// bullets, quote bars, verbatim code lines). Every line is one fixed-height
/// row, so the detail height stays analytic (lines × [`OUTPUT_LINE_HEIGHT`])
/// and the group's fold tween keeps working without measurement.
pub(super) fn thought_lines(tree: &BlockTree) -> Vec<Vec<InlineRun>> {
    let mut out: Vec<Vec<InlineRun>> = Vec::new();
    for top in &tree.blocks {
        if !out.is_empty() {
            // One blank separator row between top-level blocks (the old
            // plain-text wrap kept paragraph gaps the same way).
            out.push(Vec::new());
        }
        thought_block_lines(&top.block, 0, &mut out);
    }
    while out
        .last()
        .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
    {
        out.pop();
    }
    out
}

/// The slot-0 indent run every emitted thought line opens with (possibly
/// empty). List/quote handlers rewrite it in place to plant markers/bars, so
/// it must exist even at zero indent.
fn indent_run(indent: usize) -> Vec<InlineRun> {
    vec![InlineRun {
        text: " ".repeat(indent),
        style: InlineStyle::default(),
    }]
}

/// Append text to a line's run list, merging into the tail run when styles
/// match (keeps run counts small for the shaper).
fn push_styled(line: &mut Vec<InlineRun>, text: &str, style: &InlineStyle) {
    if text.is_empty() {
        return;
    }
    match line.last_mut() {
        Some(last) if last.style == *style => last.text.push_str(text),
        _ => line.push(InlineRun {
            text: text.to_owned(),
            style: style.clone(),
        }),
    }
}

/// Close a wrapped line: the slot-0 indent run in front (see [`indent_run`]).
fn finish_line(indent: usize, mut line: Vec<InlineRun>) -> Vec<InlineRun> {
    let mut full = indent_run(indent);
    full.append(&mut line);
    full
}

/// Word-wrap styled runs at the thought column budget. Char-counted like
/// every detail wrap — block heights must stay analytic — with words glued
/// across style boundaries (`**bold**tail` wraps as one unit), separator
/// spaces riding the preceding run, and pathological overlong tokens
/// hard-split at the budget. Hard breaks (`\n` runs) split into
/// separately-wrapped segments.
fn wrap_styled_runs(runs: &[InlineRun], indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    let budget = THOUGHT_WRAP_COLS.saturating_sub(indent).max(16);
    let mut segments: Vec<Vec<InlineRun>> = vec![Vec::new()];
    for run in runs {
        for (ix, piece) in run.text.split('\n').enumerate() {
            if ix > 0 {
                segments.push(Vec::new());
            }
            if !piece.is_empty() {
                segments.last_mut().unwrap().push(InlineRun {
                    text: piece.to_owned(),
                    style: run.style.clone(),
                });
            }
        }
    }
    for segment in segments {
        // Tokens: maximal non-whitespace piece lists, glued across run
        // boundaries so a word split by styling never wraps mid-word.
        let mut tokens: Vec<Vec<InlineRun>> = Vec::new();
        let mut in_token = false;
        for run in &segment {
            let text = run.text.as_str();
            let mut pos = 0;
            while pos < text.len() {
                let rest = &text[pos..];
                let ws = rest.chars().next().is_some_and(char::is_whitespace);
                let end = rest
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace() != ws)
                    .map_or(text.len(), |(i, _)| pos + i);
                if ws {
                    in_token = false;
                } else {
                    if !in_token {
                        tokens.push(Vec::new());
                        in_token = true;
                    }
                    push_styled(tokens.last_mut().unwrap(), &text[pos..end], &run.style);
                }
                pos = end;
            }
        }
        let mut line: Vec<InlineRun> = Vec::new();
        let mut len = 0usize;
        for token in tokens {
            let tok_len: usize = token.iter().map(|r| r.text.chars().count()).sum();
            if tok_len > budget {
                // Hard-split a pathological token at the budget.
                if len > 0 {
                    out.push(finish_line(indent, std::mem::take(&mut line)));
                    len = 0;
                }
                for piece in token {
                    let mut chars = piece.text.chars();
                    loop {
                        let chunk: String = chars.by_ref().take(budget - len).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        len += chunk.chars().count();
                        push_styled(&mut line, &chunk, &piece.style);
                        if len == budget {
                            out.push(finish_line(indent, std::mem::take(&mut line)));
                            len = 0;
                        }
                    }
                }
                continue;
            }
            if len > 0 && len + 1 + tok_len > budget {
                out.push(finish_line(indent, std::mem::take(&mut line)));
                len = 0;
            }
            if len > 0 {
                if let Some(last) = line.last_mut() {
                    last.text.push(' ');
                }
                len += 1;
            }
            for piece in token {
                push_styled(&mut line, &piece.text, &piece.style);
            }
            len += tok_len;
        }
        if len > 0 {
            out.push(finish_line(indent, line));
        }
    }
}

/// One markdown block into thought detail lines, `indent` spaces deep.
fn thought_block_lines(block: &Block, indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    match block {
        Block::Paragraph { runs } => wrap_styled_runs(runs, indent, out),
        Block::Heading { runs, .. } => {
            // Headings keep the detail's single type size — bold is the cue
            // (an 18px line box can't host display sizes).
            let bold: Vec<InlineRun> = runs
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r.style.bold = true;
                    r
                })
                .collect();
            wrap_styled_runs(&bold, indent, out);
        }
        Block::CodeBlock { code, .. } => {
            let style = InlineStyle {
                code: true,
                ..InlineStyle::default()
            };
            for line in code.lines() {
                for chunk in wrap_cols(line, THOUGHT_WRAP_COLS.saturating_sub(indent).max(16)) {
                    let mut row = indent_run(indent);
                    if !chunk.is_empty() {
                        row.push(InlineRun {
                            text: chunk.to_string(),
                            style: style.clone(),
                        });
                    }
                    out.push(row);
                }
            }
        }
        Block::List {
            ordered_start,
            items,
        } => {
            // Tight rendering: no blank rows inside a list.
            for (ix, item) in items.iter().enumerate() {
                let marker = match ordered_start {
                    Some(start) => format!("{}. ", start + ix as u64),
                    None => "• ".to_string(),
                };
                let inner = indent + marker.chars().count();
                let mark = out.len();
                for child in item {
                    thought_block_lines(child, inner, out);
                }
                if out.len() == mark {
                    // An empty item still shows its marker.
                    out.push(indent_run(inner));
                }
                // The item's first line trades its indent spaces for the
                // marker (the slot-0 run is always the indent).
                if let Some(first) = out[mark].first_mut() {
                    first.text = format!("{}{marker}", " ".repeat(indent));
                }
            }
        }
        Block::BlockQuote { children } => {
            let mark = out.len();
            for (ix, child) in children.iter().enumerate() {
                if ix > 0 {
                    out.push(Vec::new());
                }
                thought_block_lines(child, indent + 2, out);
            }
            // Trade the two quote-indent spaces for the bar on every quoted
            // line — replace, not overwrite: nested list handlers already
            // planted markers after their own deeper indent.
            for line in &mut out[mark..] {
                if let Some(first) = line.first_mut()
                    && first.text.len() >= indent + 2
                {
                    first.text.replace_range(indent..indent + 2, "│ ");
                }
            }
        }
        Block::Table { header, rows, .. } => {
            // A thought is a record, not a layout surface: cells joined with
            // a dot separator, header bold — no column machinery.
            let join = |cells: &[Vec<InlineRun>], bold: bool| -> Vec<InlineRun> {
                let mut line: Vec<InlineRun> = Vec::new();
                for (ix, cell) in cells.iter().enumerate() {
                    if ix > 0 {
                        push_styled(&mut line, " · ", &InlineStyle::default());
                    }
                    for r in cell {
                        let mut r = r.clone();
                        r.style.bold |= bold;
                        line.push(r);
                    }
                }
                line
            };
            wrap_styled_runs(&join(header, true), indent, out);
            for row in rows {
                wrap_styled_runs(&join(row, false), indent, out);
            }
        }
        Block::Rule => {
            let mut row = indent_run(indent);
            row.push(InlineRun {
                text: "———".into(),
                style: InlineStyle::default(),
            });
            out.push(row);
        }
    }
}

/// How [`parse_for_row`] produced its tree — carries the incremental parser's
/// work counters so callers (and tests) can see that per-append parse work is
/// bounded by the reparsed tail, never the whole accumulated reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live [`IncrementalParser`] advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched (render caches stay valid).
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache (no parse at all).
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring, extracted for testability: one call
/// per text part per sync. Streaming parts keep one [`IncrementalParser`] per
/// row key and advance it with the full accumulated text (`set_text` takes the
/// O(tail) append path for the prefix-extensions the doc watch delivers);
/// completed parts hit the settled cache, adopt the live parser's tree on the
/// live→complete flip (flicker-free handoff), or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            // Display tree: hanging inline markers mended so closers arriving
            // later never reflow painted text (markdown/mend.rs). Completed
            // rows below use the canonical tree — the honest settle.
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        // On the live→complete flip reuse the live parser's tree when
        // the sources match — the split rows then share the exact tree
        // the unsplit row painted, guaranteeing a flicker-free handoff.
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- streaming parse wiring (the transcript side, not the parser) ----

    #[test]
    fn live_row_parse_work_is_bounded_per_commit() {
        // Drive the EXACT wiring `rows_for` uses (`parse_for_row`) with the
        // prefix-extending commit snapshots the doc watch delivers, and prove
        // the per-commit parse work stays O(reparsed tail): a full-reparse
        // wiring would feed ~N/2 × final_len bytes through the parser across N
        // commits; the incremental path stays within a small multiple of the
        // final length regardless of N.
        let mut live_parsers = HashMap::new();
        let mut tree_cache = HashMap::new();
        let paragraph = "A paragraph of streaming prose that keeps arriving.\n\n";
        let commits = 120usize;
        let mut text = String::new();
        let mut total_parsed = 0usize;
        for i in 0..commits {
            // Each commit appends ~half a paragraph (crosses block boundaries).
            let chunk = &paragraph[..paragraph.len() / 2];
            text.push_str(if i % 2 == 0 {
                chunk
            } else {
                &paragraph[paragraph.len() / 2..]
            });
            let (tree, outcome) =
                parse_for_row(true, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
            assert!(!tree.blocks.is_empty());
            let ParseOutcome::Incremental {
                parsed_bytes,
                stable_prefix_blocks,
            } = outcome
            else {
                panic!("streaming commit must take the incremental path");
            };
            total_parsed += parsed_bytes;
            // Per commit: never a full reparse once the doc has grown past the
            // tail window (last two complete blocks + the partial trailing
            // one + the delta ≤ 3 paragraphs here).
            assert!(
                parsed_bytes <= 3 * paragraph.len(),
                "commit {i}: parsed {parsed_bytes} bytes — not bounded by the tail window"
            );
            // The stable prefix grows with the doc — settled blocks are never
            // re-touched (this is what keeps render caches valid).
            assert!(stable_prefix_blocks + 2 >= tree.blocks.len().saturating_sub(1));
        }
        // Across the whole stream: work is commits × O(tail), an order of
        // magnitude under the ~commits × len/2 a full-reparse wiring costs.
        let final_len = text.len();
        let full_reparse_cost = commits * final_len / 2;
        assert!(total_parsed <= commits * 3 * paragraph.len());
        assert!(
            total_parsed * 10 < full_reparse_cost,
            "total parsed {total_parsed} vs full-reparse ~{full_reparse_cost}"
        );

        // Live→complete handoff: the completed part adopts the live parser's
        // exact tree without parsing a single byte.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Handoff);
        // And the settled cache serves repeats with no work at all.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Cached);
    }

    fn thought_of(text: &str) -> Vec<Vec<InlineRun>> {
        thought_lines(&parse_full(text))
    }

    fn line_chars(line: &[InlineRun]) -> usize {
        line.iter().map(|r| r.text.chars().count()).sum()
    }

    fn line_string(line: &[InlineRun]) -> String {
        line.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn thought_wrap_is_word_aware_and_bounded() {
        let lines = thought_of("one two three");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_string(&lines[0]), "one two three");
        let long = "word ".repeat(200);
        let lines = thought_of(&long);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        assert!(lines.len() > 5);
        let pathological = "x".repeat(300);
        let lines = thought_of(&pathological);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        // A word glued across style boundaries wraps as ONE unit — no line
        // may split inside `**bold**tail`.
        let glued = format!("{} **bold**tail", "word ".repeat(30));
        let lines = thought_of(&glued);
        let joined: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        assert!(joined.iter().any(|l| l.ends_with("boldtail")), "{joined:?}");
    }

    #[test]
    fn thought_markdown_styles_instead_of_literal_markers() {
        // The exact user report: `**bold**` markers showed as glyphs.
        let lines = thought_of("**Planning rollback** then *checking* `parse` [docs](https://d)");
        assert_eq!(lines.len(), 1);
        let flat = line_string(&lines[0]);
        assert!(
            !flat.contains('*') && !flat.contains('`') && !flat.contains('['),
            "{flat}"
        );
        let line = &lines[0];
        assert!(
            line.iter()
                .any(|r| r.style.bold && r.text.contains("Planning rollback")),
            "bold run survives: {line:?}"
        );
        assert!(
            line.iter()
                .any(|r| r.style.italic && r.text.contains("checking"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.code && r.text.contains("parse"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.link.is_some() && r.text.contains("docs"))
        );
    }

    #[test]
    fn thought_blocks_flatten_structurally() {
        let lines = thought_of("# Head\n\npara\n\n- one\n- two\n\n```rust\nlet x = 1;\n```");
        let flat: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        // Heading renders bold, same size (one 18px row).
        assert!(
            lines[0]
                .iter()
                .any(|r| r.style.bold && r.text.contains("Head"))
        );
        // Blank separator rows between top-level blocks; tight list inside.
        assert_eq!(flat[1], "");
        assert_eq!(flat[2], "para");
        assert_eq!(flat[4], "• one");
        assert_eq!(flat[5], "• two");
        // Code lines verbatim, styled as code (mono at render).
        assert!(
            lines
                .last()
                .unwrap()
                .iter()
                .any(|r| r.style.code && r.text == "let x = 1;"),
            "{flat:?}"
        );
    }
}
