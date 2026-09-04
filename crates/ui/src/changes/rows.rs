//! The virtualized row and sticky-header model for the Changes pane: the
//! flattened `DiffRow` list at line granularity for both layouts, analytic
//! body heights, and the sticky file-header resolution and paint values.
//! Pure — the entity, fold state, and rendering live elsewhere.

use gpui::SharedString;

use crate::comments::{self, CommentSide, DiffComment};
use crate::theme::Theme;

use super::model::{FileDiff, file_notices, line_anchor, pair_anchors, split_pairs};
use super::{
    BODY_BOTTOM_PAD, DIFF_LINE_HEIGHT, DiffMode, FILE_HEADER_HEIGHT, HUNK_HEADER_HEIGHT,
    NOTICE_HEIGHT, STICKY_FILE_HEADER_TINT_ALPHA_DARK, STICKY_FILE_HEADER_TINT_ALPHA_LIGHT,
};

/// Analytic expanded-body height — drives the 180 ms fold tween without
/// measurement.
pub fn body_height(file: &FileDiff) -> f32 {
    body_height_with(file, &[], None, DiffMode::Unified)
}

pub fn body_height_with(
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
    mode: DiffMode,
) -> f32 {
    body_rows(0, file, comments, draft, mode)
        .iter()
        .map(|row| row.height(comments))
        .sum()
}

// ---------------------------------------------------------------------------
// Row model — the diff flattened to line granularity (pure)
// ---------------------------------------------------------------------------

/// One virtualized list row. The diff is flattened so each visible LINE is
/// its own row (Zed's editor draws exactly the visible line range the same
/// way): scrolling a 10k-line file materializes ~50 line rows per frame, not
/// one 10k-line element, and a collapsed file contributes no body rows at
/// all. Heights are the analytic constants above — no measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow {
    FileHeader {
        file: u32,
    },
    Notice {
        file: u32,
        notice: u32,
    },
    HunkHeader {
        file: u32,
        hunk: u32,
    },
    Line {
        file: u32,
        hunk: u32,
        line: u32,
        /// Flat index across the file's hunks — keys into the highlight slot.
        flat: u32,
    },
    /// One split row: the two line indices [`split_pairs`] paired. Carrying
    /// them inline keeps the pairing off the render path — it is computed
    /// once, when the body is flattened.
    SplitLine {
        file: u32,
        hunk: u32,
        left: Option<u32>,
        right: Option<u32>,
    },
    /// `card` indexes the file's own staged-comment slice, in staged order.
    CommentCard {
        file: u32,
        card: u32,
    },
    CommentDraft {
        file: u32,
    },
    /// Trailing pad closing an expanded body ([`BODY_BOTTOM_PAD`]).
    BodyPad {
        file: u32,
    },
    /// A body mid-fold-tween: one height-animated, clipped row standing in
    /// for the whole body. Only the slice that can be revealed is built —
    /// the tween never pays for off-screen lines.
    FoldingBody {
        file: u32,
    },
}

impl DiffRow {
    /// `FoldingBody` is height-animated, so it reports 0 and never lands in a
    /// height sum.
    fn height(self, comments: &[DiffComment]) -> f32 {
        match self {
            DiffRow::FileHeader { .. } => FILE_HEADER_HEIGHT,
            DiffRow::Notice { .. } => NOTICE_HEIGHT,
            DiffRow::HunkHeader { .. } => HUNK_HEADER_HEIGHT,
            DiffRow::Line { .. } | DiffRow::SplitLine { .. } => DIFF_LINE_HEIGHT,
            DiffRow::CommentCard { card, .. } => comments
                .get(card as usize)
                .map(|comment| comments::card_height(&comment.body))
                .unwrap_or(0.0),
            DiffRow::CommentDraft { .. } => comments::DRAFT_CARD_HEIGHT,
            DiffRow::BodyPad { .. } => BODY_BOTTOM_PAD,
            DiffRow::FoldingBody { .. } => 0.0,
        }
    }
}

/// Capacity hint only — comment cards are not counted. Split pairs can only
/// shrink the line count, so the unified count is a safe hint for both.
pub fn body_row_count(file: &FileDiff) -> usize {
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    file_notices(file).len() + file.hunks.len() + lines + 1
}

pub fn body_rows(
    file_ix: u32,
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
    mode: DiffMode,
) -> Vec<DiffRow> {
    fn push_cards(
        rows: &mut Vec<DiffRow>,
        file_ix: u32,
        comments: &[DiffComment],
        draft: Option<(CommentSide, u32)>,
        anchors: &[Option<(CommentSide, u32)>],
    ) {
        for anchor in anchors.iter().flatten() {
            for (ix, comment) in comments.iter().enumerate() {
                if comment.anchor() == *anchor {
                    rows.push(DiffRow::CommentCard {
                        file: file_ix,
                        card: ix as u32,
                    });
                }
            }
            if draft == Some(*anchor) {
                rows.push(DiffRow::CommentDraft { file: file_ix });
            }
        }
    }

    let mut rows = Vec::with_capacity(body_row_count(file));
    for notice in 0..file_notices(file).len() {
        rows.push(DiffRow::Notice {
            file: file_ix,
            notice: notice as u32,
        });
    }
    let mut hunk_flat = 0u32;
    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        rows.push(DiffRow::HunkHeader {
            file: file_ix,
            hunk: hunk_ix as u32,
        });
        match mode {
            DiffMode::Unified => {
                for (line_ix, line) in hunk.lines.iter().enumerate() {
                    rows.push(DiffRow::Line {
                        file: file_ix,
                        hunk: hunk_ix as u32,
                        line: line_ix as u32,
                        flat: hunk_flat + line_ix as u32,
                    });
                    push_cards(&mut rows, file_ix, comments, draft, &[line_anchor(line)]);
                }
            }
            DiffMode::Split => {
                for (left, right) in split_pairs(&hunk.lines) {
                    rows.push(DiffRow::SplitLine {
                        file: file_ix,
                        hunk: hunk_ix as u32,
                        left,
                        right,
                    });
                    let anchors = pair_anchors(&hunk.lines, (left, right));
                    push_cards(&mut rows, file_ix, comments, draft, &anchors);
                }
            }
        }
        hunk_flat += hunk.lines.len() as u32;
    }
    rows.push(DiffRow::BodyPad { file: file_ix });
    rows
}

/// Flatten all files into rows + each file's row span (header at
/// `range.start`, body rows after it). `collapsed(ix)` folds a file to just
/// its header. `comments` is the whole staged set; each file takes its own
/// path's slice.
pub fn flatten_rows(
    files: &[FileDiff],
    comments: &[DiffComment],
    draft: Option<(&str, CommentSide, u32)>,
    mode: DiffMode,
    mut collapsed: impl FnMut(usize) -> bool,
) -> (Vec<DiffRow>, Vec<std::ops::Range<usize>>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::with_capacity(files.len());
    for (ix, file) in files.iter().enumerate() {
        let start = rows.len();
        rows.push(DiffRow::FileHeader { file: ix as u32 });
        if !collapsed(ix) {
            let file_comments: Vec<DiffComment> = comments
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let file_draft = draft
                .filter(|(path, _, _)| *path == file.path)
                .map(|(_, side, line)| (side, line));
            rows.extend(body_rows(ix as u32, file, &file_comments, file_draft, mode));
        }
        ranges.push(start..rows.len());
    }
    (rows, ranges)
}

/// The file header that should remain visible for a logical list position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StickyFileHeader {
    pub(super) file_ix: usize,
    pub(super) header_row: usize,
    pub(super) next_header_row: Option<usize>,
}

/// Resolve a sticky file header from the current flattened row ranges.
///
/// This remains independent of the rendered list so folds and diff resets
/// cannot leave a second, stale active-file state behind.
pub(super) fn sticky_file_header(
    row_ranges: &[std::ops::Range<usize>],
    item_ix: usize,
    offset_in_item: f32,
) -> Option<StickyFileHeader> {
    let file_ix = row_ranges
        .partition_point(|range| range.start <= item_ix)
        .checked_sub(1)?;
    let range = row_ranges.get(file_ix)?;

    // A reset can briefly leave ListState pointing past the replacement
    // model. Treat that frame as having no sticky header.
    if !range.contains(&item_ix) || (item_ix == range.start && offset_in_item <= 0.0) {
        return None;
    }

    Some(StickyFileHeader {
        file_ix,
        header_row: range.start,
        next_header_row: row_ranges.get(file_ix + 1).map(|range| range.start),
    })
}

/// Offset a sticky header upward as the next file header enters its slot.
pub(super) fn sticky_header_push_offset(next_header_y: Option<f32>) -> f32 {
    next_header_y
        .map(|y| (y - FILE_HEADER_HEIGHT).min(0.0))
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileHeaderPresentation {
    Row,
    Sticky,
}

impl FileHeaderPresentation {
    pub(super) fn key_prefix(self) -> &'static str {
        match self {
            Self::Row => "file-hdr",
            Self::Sticky => "sticky-file-hdr",
        }
    }

    pub(super) fn element_id(self, file_ix: usize) -> SharedString {
        let prefix = self.key_prefix();
        SharedString::from(format!("{prefix}-{file_ix}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct StickyFileHeaderPaint {
    pub(super) rest_bg: gpui::Hsla,
    pub(super) hover_bg: gpui::Hsla,
    pub(super) border: gpui::Hsla,
    pub(super) frost_tint: Option<gpui::Hsla>,
}

/// Resolve the sticky header from the diff's content plane, not the elevated
/// overlay plane used by menus and popovers.
pub(super) fn sticky_file_header_paint(theme: &Theme) -> StickyFileHeaderPaint {
    if theme.is_frost() {
        let tint_alpha = match theme.appearance {
            crate::theme::Appearance::Dark => STICKY_FILE_HEADER_TINT_ALPHA_DARK,
            crate::theme::Appearance::Light => STICKY_FILE_HEADER_TINT_ALPHA_LIGHT,
        };
        StickyFileHeaderPaint {
            rest_bg: theme.ink(0.025),
            hover_bg: theme.glass_hover(),
            border: theme.border,
            frost_tint: Some(theme.bg.opacity(tint_alpha)),
        }
    } else {
        StickyFileHeaderPaint {
            rest_bg: crate::theme::flatten(theme.ink(0.025), theme.bg),
            hover_bg: crate::theme::flatten(theme.element_hover, theme.bg),
            border: theme.border,
            frost_tint: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{GUTTER_WIDTH, gutter_width, parse_patch, truncate_file_lines};

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
 }
@@ -10,2 +11,2 @@
 // tail
-old_line
+new_line
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
new file mode 100644
Binary files /dev/null and b/img.png differ
diff --git a/old_name.rs b/new_name.rs
similarity index 90%
rename from old_name.rs
rename to new_name.rs
";

    #[test]
    fn rows_flatten_to_line_granularity() {
        let files = parse_patch(PATCH);
        let (rows, ranges) = flatten_rows(&files, &[], None, DiffMode::Unified, |_| false);
        assert_eq!(ranges.len(), files.len());
        // Every file's span starts with its header…
        for (ix, range) in ranges.iter().enumerate() {
            assert_eq!(rows[range.start], DiffRow::FileHeader { file: ix as u32 });
            // …and spans exactly header + analytic body rows.
            assert_eq!(range.len(), 1 + body_row_count(&files[ix]));
        }
        // Spans tile the whole row vec.
        assert_eq!(ranges.last().unwrap().end, rows.len());

        // src/main.rs: header, 2 hunk headers, 8 lines, pad.
        let main_rows = &rows[ranges[0].clone()];
        assert_eq!(main_rows.len(), 1 + 2 + 8 + 1);
        assert_eq!(main_rows[1], DiffRow::HunkHeader { file: 0, hunk: 0 });
        // Flat line indices run across hunks (they key the highlight slot).
        let flats: Vec<u32> = main_rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::Line { flat, .. } => Some(*flat),
                _ => None,
            })
            .collect();
        assert_eq!(flats, (0..8).collect::<Vec<u32>>());
        assert_eq!(*main_rows.last().unwrap(), DiffRow::BodyPad { file: 0 });

        // A collapsed file contributes its header row only.
        let (rows, ranges) = flatten_rows(&files, &[], None, DiffMode::Unified, |ix| ix == 0);
        assert_eq!(ranges[0].len(), 1);
        assert_eq!(rows[ranges[1].start], DiffRow::FileHeader { file: 1 });

        // Notices lead the body: the added file carries "New file".
        let added_rows = &rows[ranges[1].clone()];
        assert_eq!(added_rows[1], DiffRow::Notice { file: 1, notice: 0 });
    }

    #[test]
    fn sticky_header_tracks_the_logical_top_row() {
        let ranges = vec![0..4, 4..5, 5..10];

        assert_eq!(sticky_file_header(&[], 0, 0.0), None);
        assert_eq!(sticky_file_header(&ranges, 0, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 0, 0.5),
            Some(StickyFileHeader {
                file_ix: 0,
                header_row: 0,
                next_header_row: Some(4),
            })
        );
        assert_eq!(
            sticky_file_header(&ranges, 2, 0.0),
            Some(StickyFileHeader {
                file_ix: 0,
                header_row: 0,
                next_header_row: Some(4),
            })
        );

        // Landing exactly on a new header hands ownership to that file; its
        // real row remains visible until it starts crossing the viewport.
        assert_eq!(sticky_file_header(&ranges, 4, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 4, 1.0),
            Some(StickyFileHeader {
                file_ix: 1,
                header_row: 4,
                next_header_row: Some(5),
            })
        );
        assert_eq!(sticky_file_header(&ranges, 5, 0.0), None);
        assert_eq!(
            sticky_file_header(&ranges, 8, 0.0),
            Some(StickyFileHeader {
                file_ix: 2,
                header_row: 5,
                next_header_row: None,
            })
        );
        assert_eq!(sticky_file_header(&ranges, 10, 0.0), None);
    }

    #[test]
    fn sticky_header_is_pushed_by_the_next_file() {
        assert_eq!(sticky_header_push_offset(None), 0.0);
        assert_eq!(sticky_header_push_offset(Some(80.0)), 0.0);
        assert_eq!(sticky_header_push_offset(Some(FILE_HEADER_HEIGHT)), 0.0);
        assert_eq!(sticky_header_push_offset(Some(24.0)), -12.0);
        assert_eq!(sticky_header_push_offset(Some(0.0)), -FILE_HEADER_HEIGHT);
    }

    #[test]
    fn sticky_header_uses_the_content_theme_in_dark_and_light() {
        use holt_theme::{AccentSelection, SurfacePreference};

        for (appearance, variant_id) in [
            (crate::theme::Appearance::Dark, "gruvbox-dark"),
            (crate::theme::Appearance::Light, "gruvbox-light"),
        ] {
            let opaque = Theme::for_selection(
                appearance,
                variant_id,
                AccentSelection::ThemeDefault,
                SurfacePreference::Opaque,
            );
            let opaque_paint = sticky_file_header_paint(&opaque);
            assert_eq!(opaque_paint.frost_tint, None, "{variant_id}");
            assert_eq!(
                opaque_paint.rest_bg,
                crate::theme::flatten(opaque.ink(0.025), opaque.bg),
                "{variant_id} opaque background"
            );
            assert_eq!(
                opaque_paint.hover_bg,
                crate::theme::flatten(opaque.element_hover, opaque.bg),
                "{variant_id} opaque hover"
            );
            assert_eq!(opaque_paint.border, opaque.border, "{variant_id} border");

            let frosted = Theme::for_selection(
                appearance,
                variant_id,
                AccentSelection::ThemeDefault,
                SurfacePreference::Frosted,
            );
            let frosted_paint = sticky_file_header_paint(&frosted);
            if frosted.is_frost() {
                let expected_alpha = match appearance {
                    crate::theme::Appearance::Dark => STICKY_FILE_HEADER_TINT_ALPHA_DARK,
                    crate::theme::Appearance::Light => STICKY_FILE_HEADER_TINT_ALPHA_LIGHT,
                };
                let tint = frosted.bg.opacity(expected_alpha);
                assert_eq!(
                    frosted_paint.frost_tint,
                    Some(tint),
                    "{variant_id} content-plane tint"
                );
                assert_ne!(
                    tint,
                    frosted.glass_overlay(),
                    "{variant_id} must not borrow the elevated overlay plane"
                );
                assert_eq!(tint.a, expected_alpha, "{variant_id} tint coverage");
                assert_eq!(
                    frosted_paint.hover_bg,
                    frosted.glass_hover(),
                    "{variant_id} themed hover"
                );
            } else {
                assert_eq!(frosted_paint.frost_tint, None, "{variant_id}");
            }
            assert_eq!(
                frosted_paint.border, frosted.border,
                "{variant_id} frosted border"
            );
        }
    }

    #[test]
    fn split_flattening_pairs_rows_and_keeps_heights_analytic() {
        let files = parse_patch(PATCH);
        let (rows, ranges) = flatten_rows(&files, &[], None, DiffMode::Split, |_| false);
        assert_eq!(ranges.len(), files.len());
        assert_eq!(ranges.last().unwrap().end, rows.len());

        // src/main.rs: header, 2 hunk headers, 4 + 2 paired rows, pad — the
        // same 8 lines, two columns.
        let main_rows = &rows[ranges[0].clone()];
        assert_eq!(main_rows.len(), 1 + 2 + (4 + 2) + 1);
        assert_eq!(
            main_rows[2],
            DiffRow::SplitLine {
                file: 0,
                hunk: 0,
                left: Some(0),
                right: Some(0),
            }
        );
        assert_eq!(
            main_rows[4],
            DiffRow::SplitLine {
                file: 0,
                hunk: 0,
                left: None,
                right: Some(3),
            }
        );
        assert_eq!(*main_rows.last().unwrap(), DiffRow::BodyPad { file: 0 });
        // Pairing only ever merges rows, so split is never the taller layout.
        assert!(main_rows.len() < 1 + body_row_count(&files[0]));

        // Heights stay analytic — the fold tween needs no measurement.
        assert_eq!(
            body_height_with(&files[0], &[], None, DiffMode::Split),
            2.0 * HUNK_HEADER_HEIGHT + 6.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
    }

    #[test]
    fn split_rows_carry_the_comments_of_both_columns() {
        let files = parse_patch(PATCH);
        // A context row must not stack the same card twice.
        let comment = DiffComment::new("src/main.rs", CommentSide::New, 1, "why");
        let rows = body_rows(0, &files[0], &[comment], None, DiffMode::Split);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, DiffRow::CommentCard { .. }))
                .count(),
            1
        );

        // Both sides of one paired row hang off that row, in column order.
        let staged = vec![
            DiffComment::new("src/main.rs", CommentSide::Old, 2, "left"),
            DiffComment::new("src/main.rs", CommentSide::New, 2, "right"),
        ];
        let rows = body_rows(0, &files[0], &staged, None, DiffMode::Split);
        let edit = rows
            .iter()
            .position(|row| matches!(row, DiffRow::SplitLine { left: Some(1), .. }))
            .unwrap();
        assert_eq!(rows[edit + 1], DiffRow::CommentCard { file: 0, card: 0 });
        assert_eq!(rows[edit + 2], DiffRow::CommentCard { file: 0, card: 1 });
    }

    #[test]
    fn gutters_fit_the_largest_line_number() {
        let files = parse_patch(PATCH);
        // src/main.rs second hunk ends at old 11 / new 12.
        assert_eq!(files[0].max_line, 12);
        assert_eq!(gutter_width(&files[0]), GUTTER_WIDTH);

        // Every digit count keeps ≥6px clear of the accent bar on the left
        // of the number (digits×6.6 + 8px right pad + 6px gap), and the
        // column never shrinks below the classic 36px.
        let mut file = files[0].clone();
        for digits in 1..=7u32 {
            file.max_line = 10u32.pow(digits) - 1;
            let w = gutter_width(&file);
            assert!(w >= GUTTER_WIDTH);
            let left_gap = w - (digits as f32 * 6.6 + 8.0);
            assert!(
                left_gap >= 6.0,
                "{digits} digits: left gap {left_gap} < 6px"
            );
        }
        // 4 digits outgrow the classic column now (the old formula left
        // them 1.6px off the bar — visually touching).
        file.max_line = 9999;
        assert!(gutter_width(&file) > GUTTER_WIDTH);
        file.max_line = 27404;
        assert!(
            gutter_width(&file)
                > gutter_width(&{
                    let mut f = file.clone();
                    f.max_line = 9999;
                    f
                })
        );

        // Truncation refits the gutter to what actually renders: the first
        // 3 lines are ctx(1,1) / del(2,·) / add(·,2) — max line 2.
        let mut file = files[0].clone();
        truncate_file_lines(&mut file, 3);
        assert_eq!(file.max_line, 2);
    }

    #[test]
    fn body_height_is_analytic() {
        let files = parse_patch(PATCH);
        let main = &files[0];
        let lines: usize = main.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(
            body_height(main),
            2.0 * HUNK_HEADER_HEIGHT + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
        // Notices add height (added file: 1 notice + meta line inside hunk).
        let added = &files[1];
        assert_eq!(
            body_height(added),
            NOTICE_HEIGHT + HUNK_HEADER_HEIGHT + 3.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
    }
}
