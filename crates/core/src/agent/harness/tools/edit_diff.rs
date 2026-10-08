//! Port of `pi-core/agent/src/harness/tools/edit-diff.ts`.
//!
//! The TypeScript module computes line diffs through the npm `diff`
//! package; the Rust port implements the same line-LCS and unified-patch
//! rendering (verified against the package's output for the suite's
//! inputs — hunk counts are always printed, `0` counts address the line
//! before the range, and headers are file names only).

use unicode_normalization::UnicodeNormalization;

/// Port of `detectLineEnding`.
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// Port of `normalizeToLF`.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Port of `restoreLineEndings`.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Port of `normalizeForFuzzyMatch` (NFKC, trailing whitespace, smart
/// punctuation, dashes, and spaces).
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let normalized: String = text.nfkc().collect();
    let trailing_stripped: Vec<&str> = normalized
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();
    let joined = trailing_stripped.join("\n");
    let mut out = String::with_capacity(joined.len());
    for character in joined.chars() {
        match character {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => out.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => out.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => out.push('-'),
            '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => {
                out.push(' ')
            }
            other => out.push(other),
        }
    }
    out
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

#[derive(Clone, Copy, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
pub struct TextReplacement {
    pub match_index: usize,
    pub match_length: usize,
    pub new_text: String,
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn get_replacement_line_range(lines: &[LineSpan], replacement: &TextReplacement) -> (usize, usize) {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let mut start_line = usize::MAX;
    for (index, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = index;
            break;
        }
    }
    if start_line == usize::MAX {
        panic!("Replacement range is outside the base content.");
    }

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        panic!("Replacement range is outside the base content.");
    }

    (start_line, end_line + 1)
}

fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index.saturating_sub(offset);
        let end = (match_index + replacement.match_length).min(result.len());
        result.replace_range(match_index..end, &replacement.new_text);
    }
    result
}

/// Port of `applyReplacementsPreservingUnchangedLines`.
pub fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> String {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        panic!(
            "Cannot preserve unchanged lines because the base content has a different line count."
        );
    }

    let mut sorted_replacements: Vec<&TextReplacement> = replacements.iter().collect();
    sorted_replacements.sort_by_key(|replacement| replacement.match_index);
    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for replacement in sorted_replacements {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, replacement);
        if let Some(current) = groups.last_mut()
            && start_line < current.1
        {
            current.1 = current.1.max(end_line);
            current.2.push((*replacement).clone());
            continue;
        }
        groups.push((start_line, end_line, vec![(*replacement).clone()]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (start_line, end_line, replacements) in groups {
        result.push_str(&original_lines[original_line_index..start_line].concat());
        let group_start_offset = base_lines[start_line].start;
        let group_end_offset = base_lines[end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &replacements,
            group_start_offset,
        ));
        original_line_index = end_line;
    }
    result.push_str(&original_lines[original_line_index..].concat());
    result
}

/// Port of `FuzzyMatchResult`.
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: String,
}

/// Port of `Edit`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// Port of `fuzzyFindText`.
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(exact_index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: exact_index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    let fuzzy_index = fuzzy_content.find(&fuzzy_old_text);
    match fuzzy_index {
        None => FuzzyMatchResult {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
        Some(fuzzy_index) => FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
    }
}

/// Port of `stripBom`.
pub fn strip_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{feff}') {
        Some(text) => ("\u{feff}", text),
        None => ("", content),
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.split(&fuzzy_old_text).count() - 1
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

// Deliberate deviation from TypeScript v0.84.4: `applyEditsToNormalizedContent`
// throws the bare `getNotFoundError` message above. The Rust port appends
// failure-path diagnostics — a sequential-edit-dependency diagnosis and the
// closest candidate region — so the model can recover in one retry instead of
// re-reading the file and guessing. Success-path behavior (which edits apply,
// resulting content, all other error strings) is unchanged; when no diagnostic
// fires the error string is byte-identical to TypeScript. Focused tests in
// `tests/harness_tools.rs` pin both the enriched and the bare messages.

/// Upper bound on sliding-window positions scored by the candidate hint; past
/// this the hint is skipped rather than paying a full extra scan of a huge file.
const MAX_CANDIDATE_WINDOWS: usize = 100_000;
const CANDIDATE_PROBE_LINES: usize = 3;
const CANDIDATE_MIN_SIMILARITY: f64 = 0.6;
const CANDIDATE_MAX_LINE_CHARS: usize = 200;

/// Multiset similarity of two lines (sorted-byte intersection over the longer
/// length). Order-insensitive by design: the hint must still locate the region
/// when only the ordering within a line or its indentation differs.
fn line_similarity(a: &str, b: &str) -> f64 {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut sorted_a = a.to_vec();
    let mut sorted_b = b.to_vec();
    sorted_a.sort_unstable();
    sorted_b.sort_unstable();
    let mut common = 0usize;
    let mut i = 0;
    let mut j = 0;
    while i < sorted_a.len() && j < sorted_b.len() {
        match sorted_a[i].cmp(&sorted_b[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    common as f64 / a.len().max(b.len()) as f64
}

/// Locate the region of `base_content` most similar to the head of `old_text`
/// and render it as a hint, or `None` when nothing is close enough to point at.
fn closest_candidate_hint(base_content: &str, old_text: &str) -> Option<String> {
    let probe: Vec<&str> = old_text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .take(CANDIDATE_PROBE_LINES)
        .collect();
    if probe.is_empty() {
        return None;
    }
    let lines: Vec<&str> = base_content.split('\n').collect();
    if lines.len() < probe.len() || lines.len() + 1 - probe.len() > MAX_CANDIDATE_WINDOWS {
        return None;
    }

    let mut best_start = 0;
    let mut best_score = 0.0;
    for start in 0..=(lines.len() - probe.len()) {
        let score = probe
            .iter()
            .enumerate()
            .map(|(offset, probe_line)| line_similarity(probe_line, lines[start + offset]))
            .sum::<f64>()
            / probe.len() as f64;
        if score > best_score {
            best_score = score;
            best_start = start;
        }
    }
    if best_score < CANDIDATE_MIN_SIMILARITY {
        return None;
    }

    // Display exactly the matched window so the reported line range is the
    // region that scored best, not an arbitrary number of surrounding lines.
    let shown = probe.len();
    let first_line = best_start + 1;
    let last_line = first_line + shown - 1;
    let mut hint =
        format!("The closest match in the file is around lines {first_line}-{last_line}:");
    for (offset, line) in lines.iter().skip(best_start).take(shown).enumerate() {
        let display: String = line.chars().take(CANDIDATE_MAX_LINE_CHARS).collect();
        hint.push_str(&format!("\n  {} | {display}", first_line + offset));
    }
    hint.push_str(
        "\nRe-read that region with the read tool and copy the text exactly, including indentation and newlines.",
    );
    Some(hint)
}

/// Detect that `edits[edit_index]` was written against the result of applying
/// `edits[0..edit_index]` in order, rather than against the original content.
/// `None` when the hypothesis does not hold exactly.
fn sequential_edits_diagnostic(
    base_content: &str,
    edits: &[Edit],
    edit_index: usize,
) -> Option<String> {
    if edit_index == 0 {
        return None;
    }
    let mut current = base_content.to_string();
    for edit in &edits[..edit_index] {
        let index = current.find(edit.old_text.as_str())?;
        current.replace_range(index..index + edit.old_text.len(), &edit.new_text);
    }
    current.find(edits[edit_index].old_text.as_str())?;
    Some(format!(
        "edits[{edit_index}].oldText matches only after edits[0..{edit_index}] are applied in order, but every edit is matched against the same original file content, not the result of earlier edits. Merge the dependent edits into one edit whose oldText comes from the original file, or apply them in separate edit calls."
    ))
}

fn not_found_error_with_diagnostics(
    base_content: &str,
    edits: &[Edit],
    edit_index: usize,
    path: &str,
) -> String {
    let base = not_found_error(path, edit_index, edits.len());
    if let Some(diagnostic) = sequential_edits_diagnostic(base_content, edits, edit_index) {
        return format!("{base} {diagnostic}");
    }
    if let Some(hint) = closest_candidate_hint(base_content, &edits[edit_index].old_text) {
        return format!("{base} {hint}");
    }
    base
}

fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// Port of `AppliedEditsResult`.
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

/// Port of `applyEditsToNormalizedContent`. Errors are the TypeScript
/// message strings.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, index, normalized_edits.len()));
        }
    }

    let used_fuzzy_match = normalized_edits
        .iter()
        .any(|edit| fuzzy_find_text(normalized_content, &edit.old_text).used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<(usize, TextReplacement)> = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(not_found_error_with_diagnostics(
                &replacement_base_content,
                &normalized_edits,
                index,
                path,
            ));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(
                path,
                index,
                normalized_edits.len(),
                occurrences,
            ));
        }
        matched_edits.push((
            index,
            TextReplacement {
                match_index: match_result.index,
                match_length: match_result.match_length,
                new_text: edit.new_text.clone(),
            },
        ));
    }

    matched_edits.sort_by_key(|(_, replacement)| replacement.match_index);
    for window in matched_edits.windows(2) {
        let (previous_index, previous) = &window[0];
        let (current_index, current) = &window[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{previous_index}] and edits[{current_index}] overlap in {path}. Merge them into one edit or target disjoint regions."
            ));
        }
    }

    let replacements: Vec<TextReplacement> = matched_edits
        .into_iter()
        .map(|(_, replacement)| replacement)
        .collect();
    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &replacements,
        )
    } else {
        apply_replacements(&replacement_base_content, &replacements, 0)
    };

    if base_content == new_content {
        return Err(no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

// ---------------------------------------------------------------------------
// Line diff (npm `diff` package equivalents)
// ---------------------------------------------------------------------------

/// A `diffLines` part: a run of equal, added, or removed lines.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffPart {
    pub value: String,
    pub count: usize,
    pub added: bool,
    pub removed: bool,
}

/// Port of `Diff.diffLines` (LCS over newline-terminated lines, equal runs
/// merged with a count).
pub fn diff_lines(old_content: &str, new_content: &str) -> Vec<DiffPart> {
    let old_lines = split_lines_with_endings(old_content);
    let new_lines = split_lines_with_endings(new_content);

    // LCS table over line indices.
    let mut table = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
    for i in (0..old_lines.len()).rev() {
        for j in (0..new_lines.len()).rev() {
            table[i][j] = if old_lines[i] == new_lines[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    enum Op<'a> {
        Equal(&'a str),
        Remove(&'a str),
        Add(&'a str),
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < old_lines.len() && j < new_lines.len() {
        if old_lines[i] == new_lines[j] {
            ops.push(Op::Equal(old_lines[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(Op::Remove(old_lines[i]));
            i += 1;
        } else {
            ops.push(Op::Add(new_lines[j]));
            j += 1;
        }
    }
    while i < old_lines.len() {
        ops.push(Op::Remove(old_lines[i]));
        i += 1;
    }
    while j < new_lines.len() {
        ops.push(Op::Add(new_lines[j]));
        j += 1;
    }

    // Merge into parts: consecutive removes followed by adds form the
    // removed-then-added pair the jsdiff emitter produces.
    let mut parts: Vec<DiffPart> = Vec::new();
    let mut index = 0;
    while index < ops.len() {
        match ops[index] {
            Op::Equal(line) => {
                let mut value = line.to_string();
                let mut count = 1;
                index += 1;
                while let Op::Equal(next) = ops[index.min(ops.len() - 1)] {
                    if index >= ops.len() {
                        break;
                    }
                    value.push_str(next);
                    count += 1;
                    index += 1;
                }
                parts.push(DiffPart {
                    value,
                    count,
                    added: false,
                    removed: false,
                });
            }
            Op::Remove(_) => {
                let mut removed = String::new();
                let mut removed_count = 0;
                while index < ops.len() {
                    if let Op::Remove(line) = ops[index] {
                        removed.push_str(line);
                        removed_count += 1;
                        index += 1;
                    } else {
                        break;
                    }
                }
                let mut added = String::new();
                let mut added_count = 0;
                while index < ops.len() {
                    if let Op::Add(line) = ops[index] {
                        added.push_str(line);
                        added_count += 1;
                        index += 1;
                    } else {
                        break;
                    }
                }
                if removed_count > 0 {
                    parts.push(DiffPart {
                        value: removed,
                        count: removed_count,
                        added: false,
                        removed: true,
                    });
                }
                if added_count > 0 {
                    parts.push(DiffPart {
                        value: added,
                        count: added_count,
                        added: true,
                        removed: false,
                    });
                }
            }
            Op::Add(line) => {
                let mut added = line.to_string();
                let mut count = 1;
                index += 1;
                while index < ops.len() {
                    if let Op::Add(next) = ops[index] {
                        added.push_str(next);
                        count += 1;
                        index += 1;
                    } else {
                        break;
                    }
                }
                parts.push(DiffPart {
                    value: added,
                    count,
                    added: true,
                    removed: false,
                });
            }
        }
    }
    parts
}

/// Port of `Diff.createTwoFilesPatch` with `FILE_HEADERS_ONLY`.
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let parts = diff_lines(old_content, new_content);

    // Flatten to per-line ops with old/new line numbers.
    #[derive(Clone, Copy)]
    enum LineOp {
        Context,
        Remove,
        Add,
    }
    let mut ops: Vec<(LineOp, &str)> = Vec::new();
    for part in &parts {
        for line in split_lines_with_endings(&part.value) {
            let op = if part.added {
                LineOp::Add
            } else if part.removed {
                LineOp::Remove
            } else {
                LineOp::Context
            };
            ops.push((op, line));
        }
    }

    // Mark changed regions, then group into hunks with context.
    let changed: Vec<bool> = ops
        .iter()
        .map(|(op, _)| !matches!(op, LineOp::Context))
        .collect();
    let mut hunks: Vec<(usize, usize)> = Vec::new(); // [start, end) in ops
    let mut index = 0;
    while index < ops.len() {
        if !changed[index] {
            index += 1;
            continue;
        }
        let start = index.saturating_sub(context_lines);
        let mut end = index + 1;
        // Extend over the change run and trailing context.
        while end < ops.len() && changed[end] {
            end += 1;
        }
        end = (end + context_lines).min(ops.len());
        // Merge with a previous hunk when ranges overlap.
        if let Some(last) = hunks.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            hunks.push((start, end));
        }
        index = end;
    }

    let mut patch = format!("--- {path}\n+++ {path}\n");
    for (start, end) in hunks {
        // Old-side and new-side ranges.
        let mut old_start: usize = 1;
        let mut new_start: usize = 1;
        for (_op_index, (op, _)) in ops.iter().enumerate().take(start) {
            match op {
                LineOp::Add => new_start += 1,
                _ => old_start += 1,
            }
        }
        let mut old_count = 0;
        let mut new_count = 0;
        for (op, _) in &ops[start..end] {
            match op {
                LineOp::Add => new_count += 1,
                LineOp::Remove => old_count += 1,
                LineOp::Context => {
                    old_count += 1;
                    new_count += 1;
                }
            }
        }
        let old_header = if old_count == 0 {
            format!("-{},0", old_start.saturating_sub(1))
        } else {
            format!("-{old_start},{old_count}")
        };
        let new_header = if new_count == 0 {
            format!("-{},0", new_start.saturating_sub(1)).replace('-', "+")
        } else {
            format!("+{new_start},{new_count}")
        };
        patch.push_str(&format!("@@ {old_header} {new_header} @@\n"));
        for (op, line) in &ops[start..end] {
            let prefix = match op {
                LineOp::Context => ' ',
                LineOp::Remove => '-',
                LineOp::Add => '+',
            };
            patch.push(prefix);
            patch.push_str(line);
        }
    }
    patch
}

/// Port of `generateDiffString`.
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let parts = diff_lines(old_content, new_content);
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    let pad = |number: usize| format!("{number:>line_num_width$}");

    for (index, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for line in raw {
                if part.added {
                    output.push(format!("+{} {line}", pad(new_line_num)));
                    new_line_num += 1;
                } else {
                    output.push(format!("-{} {line}", pad(old_line_num)));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change =
                index < parts.len() - 1 && (parts[index + 1].added || parts[index + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in raw {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading: Vec<&str> = raw[..context_lines].to_vec();
                    let trailing: Vec<&str> = raw[raw.len() - context_lines..].to_vec();
                    let skipped = raw.len() - leading.len() - trailing.len();
                    for line in leading {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    output.push(format!(" {} ...", pad(0)));
                    old_line_num += skipped;
                    new_line_num += skipped;
                    for line in trailing {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown = &raw[..context_lines.min(raw.len())];
                let skipped = raw.len() - shown.len();
                for line in shown {
                    output.push(format!(" {} {line}", pad(old_line_num)));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                if skipped > 0 {
                    output.push(format!(" {} ...", pad(0)));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            } else if has_trailing_change {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(format!(" {} ...", pad(0)));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for line in &raw[skipped..] {
                    output.push(format!(" {} {line}", pad(old_line_num)));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    (output.join("\n"), first_changed_line)
}
