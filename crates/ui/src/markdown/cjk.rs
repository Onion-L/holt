//! CJK-friendly emphasis flanking, applied inside [`super::parse_full`].
//!
//! CommonMark's flanking rules misfire on CJK text (upstream gap, cmark#518;
//! GitHub renders the same): a `**` run between fullwidth punctuation and a
//! CJK letter loses one flank, so emphasis around a fullwidth paren can never
//! resolve and the markers stay literal — `…（v1）**问题` never closes,
//! `文**（注…` never opens. For a maximal `*` run of length ≥ 2 the two
//! failure shapes are:
//!
//! - close side: `punct|run|letter` — not right-flanking, cannot close
//! - open side:  `letter|run|punct` — not left-flanking, cannot open
//!
//! The fix inserts [`ZWSP`] on the broken side. ZWSP is neither Unicode
//! whitespace nor punctuation, so flanking treats it like a letter and the
//! run regains the flank. For `**` this is strictly additive: a run that
//! already opened or closed keeps doing so (its other flank is untouched),
//! one that couldn't gains the ability. Single-`*` runs are skipped — there
//! an insertion produces a both-flanking run, which *loses* its single-side
//! role (`（*斜*）` would stop rendering italic) — and `_` runs too (their
//! intraword conditions stay unsatisfied with either neighbor).
//!
//! The rewrite must happen inside `parse_full`: every `TopBlock` byte range
//! has to index the canonical source (streaming boundaries and `mend` slice
//! the source with them). So the pre-pass records its insertion offsets and
//! [`to_original`] maps every event range back to canonical coordinates,
//! while [`strip`] removes the inserted characters from event text — the
//! scan runs before parsing and cannot know code spans, so anything that
//! landed inside code is removed on the way out, leaving code byte-identical.
//!
//! Insertion decisions read only the characters adjacent to a run, and a
//! line-start boundary reads `\n` (whitespace) where a slice start reads
//! nothing — both classify identically — so full and tail parses always
//! insert at the same places (streaming parity).

use std::ops::Range;

use pulldown_cmark::{CowStr, Event};

const ZWSP: char = '\u{200B}';

/// Rewrite `source` for parsing: for each maximal `*` run of length ≥ 2,
/// insert a ZWSP on the side that broke flanking. Returns the rewritten text
/// plus the insertion offsets in canonical coordinates (ascending, at most
/// one per run). `None` when nothing applies; the no-`*` fast path avoids
/// all allocation.
pub(super) fn rewrite(source: &str) -> Option<(String, Vec<usize>)> {
    if !source.as_bytes().contains(&b'*') {
        return None;
    }
    let bytes = source.as_bytes();
    let mut insertions: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'*' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'*' {
                i += 1;
            }
            if i - start >= 2 {
                let before = classify(source[..start].chars().next_back());
                let after = classify(source[i..].chars().next());
                match (before, after) {
                    // punct|run|letter: insert before the run (regain close).
                    (PUNCT, TEXT) => insertions.push(start),
                    // letter|run|punct: insert after the run (regain open).
                    (TEXT, PUNCT) => insertions.push(i),
                    _ => {}
                }
            }
        } else {
            i += 1;
        }
    }
    if insertions.is_empty() {
        return None;
    }
    let mut rewritten = String::with_capacity(source.len() + insertions.len() * ZWSP.len_utf8());
    let mut last = 0;
    for &at in &insertions {
        rewritten.push_str(&source[last..at]);
        rewritten.push(ZWSP);
        last = at;
    }
    rewritten.push_str(&source[last..]);
    Some((rewritten, insertions))
}

/// Map an event byte range from rewritten coordinates back to canonical
/// ones. Insertion `j` sits at rewritten offset `insertions[j] + 3*j` (each
/// prior ZWSP adds 3 bytes); a query subtracts 3 bytes per insertion
/// strictly before it, so a ZWSP's own start maps to its canonical boundary.
pub(super) fn to_original(range: Range<usize>, insertions: Option<&[usize]>) -> Range<usize> {
    let Some(ins) = insertions else {
        return range;
    };
    let unit = ZWSP.len_utf8();
    let shift = |x: usize| {
        let before = ins
            .iter()
            .enumerate()
            .filter(|(j, at)| *at + unit * j < x)
            .count();
        x - unit * before
    };
    shift(range.start)..shift(range.end)
}

/// Remove the pre-pass's ZWSPs from event text. Clean events pass through
/// by move (no copy).
pub(super) fn strip(event: Event<'_>) -> Event<'_> {
    fn clean(t: CowStr<'_>) -> CowStr<'_> {
        if t.contains(ZWSP) {
            CowStr::from(t.replace(ZWSP, ""))
        } else {
            t
        }
    }
    match event {
        Event::Text(t) => Event::Text(clean(t)),
        Event::Code(t) => Event::Code(clean(t)),
        Event::Html(t) => Event::Html(clean(t)),
        Event::InlineHtml(t) => Event::InlineHtml(clean(t)),
        event => event,
    }
}

/// CommonMark's "Unicode punctuation character" (general category P* or S*),
/// approximated with ASCII plus the fullwidth/CJK blocks that matter here.
/// Fail-open: a missed character only means no insertion (today's rendering),
/// never a wrong one.
fn is_punct_like(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(c,
            '\u{3000}'..='\u{303F}'   // CJK symbols and punctuation: 、。〈〉《》「」…
            | '\u{FF01}'..='\u{FF0F}' // ！＂＃＄％＆＇（）＊＋，－．／
            | '\u{FF1A}'..='\u{FF20}' // ：；＜＝＞？＠
            | '\u{FF3B}'..='\u{FF40}' // ［＼］＾＿｀
            | '\u{FF5B}'..='\u{FF65}' // ｛｜｝～・
            | '\u{2013}'..='\u{2026}' // – — ' ' " " … † ‡ •
            | '\u{00B7}') // ·
}

/// Neighbor class of a delimiter run: whitespace/edge (0), punctuation (1),
/// or text — letters, CJK ideographs, digits (2).
const EDGE: u8 = 0;
const PUNCT: u8 = 1;
const TEXT: u8 = 2;

fn classify(c: Option<char>) -> u8 {
    match c {
        None => EDGE,
        Some(c) if c.is_whitespace() => EDGE,
        Some(c) if is_punct_like(c) => PUNCT,
        Some(_) => TEXT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_insertion_without_a_failure_shape() {
        assert_eq!(rewrite("没有任何星号"), None);
        assert_eq!(rewrite("词**中**词"), None); // letter|run|letter: fine
        assert_eq!(rewrite("（*斜*）"), None); // single runs are skipped
        assert_eq!(rewrite("_a_ 和 __b__"), None); // `_` runs are skipped
        assert_eq!(rewrite("**行首加粗**"), None); // edge|run|letter opens fine
        // letter|run|punct-close gets a no-op insertion (renders the same);
        // covered as a render-guard in parser tests, not here.
    }

    #[test]
    fn failure_shapes_get_one_insertion_each() {
        // Reported case: close run after `）`.
        let (rewritten, at) = rewrite("本质是**授权（v1）**问题：").unwrap();
        assert_eq!(at, vec!["本质是**授权（v1）".len()]);
        assert_eq!(rewritten, "本质是**授权（v1）\u{200b}**问题：");
        // Open run before `（`.
        let (rewritten, at) = rewrite("文**（注**释").unwrap();
        assert_eq!(at, vec!["文**".len()]);
        assert_eq!(rewritten, "文**\u{200b}（注**释");
    }

    /// The offset mapping must count ZWSPs as 3 bytes each — the naive
    /// per-insertion count sends ranges out of bounds or into mid-character
    /// slices. Pinned edge cases: boundaries exactly at, before, and after
    /// each insertion.
    #[test]
    fn ranges_map_back_exactly() {
        // canonical `a）**b`, insertion at 2: rewritten `a）\u{200b}**b`.
        let ins = [2];
        let at = |x| to_original(x..x, Some(&ins));
        assert_eq!(at(0), 0..0);
        assert_eq!(at(2), 2..2, "ZWSP start maps to its own boundary");
        // (queries inside the ZWSP never occur: no event boundary lands there)
        assert_eq!(to_original(5..7, Some(&ins)), 2..4, "the `**` run");
        assert_eq!(to_original(7..8, Some(&ins)), 4..5, "text after");
        // Two insertions with multibyte neighbors, exercising the j-th
        // insertion arithmetic; every remapped event range must slice the
        // canonical source cleanly (never mid-character, never out of
        // bounds) and the emphasis must resolve as intended.
        let src = "中**文（v）**字**（x**y";
        let (rewritten, ins) = rewrite(src).unwrap();
        assert_eq!(ins.len(), 2);
        let events: Vec<_> =
            pulldown_cmark::Parser::new_ext(&rewritten, pulldown_cmark::Options::all())
                .into_offset_iter()
                .map(|(e, r)| (strip(e), to_original(r, Some(&ins))))
                .collect();
        assert!(!events.is_empty());
        for (event, range) in &events {
            let _ = &src[range.clone()];
            if let Event::Text(t) = event {
                assert!(!t.contains(ZWSP));
            }
        }
    }
}
