//! User bubbles past [`OVERSIZED_BYTES`] paint like a viewport, not a document.
//!
//! The transcript list virtualizes rows. One row that holds a multi-megabyte
//! diff is still one element, and `StyledText` shapes and walks every glyph
//! of it on every frame that row intersects the viewport. This element keeps
//! the row (and the full string) but shapes only the lines the content mask
//! can show. Height is a column count times the bubble line height, so the
//! scrollbar still covers the whole diff.
//!
//! The column grid is the mono font's "0" advance. Tabs are four columns and
//! non-ASCII characters are two, and each visual row is shaped on its own so
//! a hard-wrapped SVG path is not one layout. Selection, links, and chips
//! stay on the `StyledText` path; a bubble this long has none of those.

use gpui::{
    App, AvailableSpace, Bounds, ContentMask, Element, GlobalElementId, IntoElement, LayoutId,
    Pixels, SharedString, Style, TextAlign, TextRun, Window, div, point, prelude::*, px, relative,
    size,
};

use crate::theme::Theme;

use super::MAX_CONTENT_WIDTH;

/// Review diffs are megabytes. Ordinary prompts, including long ones, stay on
/// `StyledText` so selection and chips keep working.
const OVERSIZED_BYTES: usize = 12_000;

/// Bubble metrics, matching the user bubble's `text_size` / `line_height`.
const FONT_PX: f32 = 14.0;
const LINE_PX: f32 = 22.0;

/// Rows shaped above and below the mask so a frame of scroll doesn't pop.
const OVERDRAW_ROWS: f32 = 2.0;

pub(super) fn oversized(text: &str) -> bool {
    text.len() > OVERSIZED_BYTES
}

pub(super) fn element(text: SharedString, width_hint: Pixels) -> gpui::AnyElement {
    div()
        .w_full()
        .min_w_0()
        .cursor(gpui::CursorStyle::IBeam)
        .overflow_hidden()
        .child(VirtualBubbleText { text, width_hint })
        .into_any_element()
}

struct VirtualBubbleText {
    text: SharedString,
    /// The bubble's real content width (row width minus gutters, capped,
    /// minus padding) as the transcript's layout probe last measured it —
    /// taffy hands the leaf the UNCLAMPED row width, which overestimates
    /// the painted width on narrow rows and used to size the row for too
    /// few wraps, clipping the diff's tail out of reach.
    width_hint: Pixels,
}

impl IntoElement for VirtualBubbleText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VirtualBubbleText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let text = self.text.clone();
        let width_hint = self.width_hint;
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let (_, line_height) = bubble_metrics(window);
                let advance = advance_of_zero(window, cx);
                let width = layout_width(known.width, available.width, width_hint);
                let rows = content_rows(&text, columns_for(width, advance));
                let reported = if known.width.is_none()
                    && matches!(available.width, AvailableSpace::MinContent)
                {
                    px(0.0)
                } else {
                    width
                };
                size(reported, px(f32::from(line_height) * rows as f32))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (font_size, line_height) = bubble_metrics(window);
        let line = f32::from(line_height);
        if line <= 0.0 || bounds.size.width < px(1.0) {
            return;
        }
        let Some((top, bottom)) = visible_span(bounds, window.content_mask(), line) else {
            return;
        };
        let advance = advance_of_zero(window, cx);
        let cols = columns_for(bounds.size.width, advance);
        let theme = Theme::of(cx);
        let font = gpui::font(theme.font_mono.clone());
        let color = theme.text;
        let mut y = 0.0f32;
        for line_text in self.text.split('\n') {
            let rows = line_rows(line_text, cols);
            let line_top = y;
            let line_bottom = y + rows as f32 * line;
            y = line_bottom;
            if line_bottom < top {
                continue;
            }
            if line_top > bottom {
                break;
            }
            let first = ((top - line_top) / line).floor().max(0.0) as usize;
            if first >= rows {
                continue;
            }
            let last = (((bottom - line_top) / line).ceil() as usize).clamp(first, rows);
            visit_rows(line_text, first, last, cols, |row, slice| {
                if slice.is_empty() {
                    return;
                }
                let run = TextRun {
                    len: slice.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(
                    SharedString::from(slice),
                    font_size,
                    &[run],
                    None,
                );
                let origin_y = f32::from(bounds.origin.y) + line_top + row as f32 * line;
                let _ = shaped.paint(
                    point(bounds.origin.x, px(origin_y)),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            });
        }
    }
}

fn bubble_metrics(window: &Window) -> (Pixels, Pixels) {
    let rem = window.rem_size();
    let font_size = crate::typography::ui_rems(FONT_PX).to_pixels(rem);
    let line_height = window.pixel_snap(crate::typography::ui_rems(LINE_PX).to_pixels(rem));
    (font_size, line_height)
}

/// The bubble's content box at its cap: `max_w(80%)` minus the 16px padding
/// on each side. Measuring wider than this (the unclamped row) would make
/// the row shorter than the clamped bubble and clip the tail.
fn content_cap() -> Pixels {
    px(MAX_CONTENT_WIDTH * 0.8 - 32.0)
}

fn layout_width(known: Option<Pixels>, available: AvailableSpace, hint: Pixels) -> Pixels {
    let cap = content_cap();
    let raw = known
        .or_else(|| (hint > px(0.0)).then_some(hint))
        .unwrap_or(match available {
            AvailableSpace::Definite(width) => width,
            AvailableSpace::MinContent | AvailableSpace::MaxContent => cap,
        });
    raw.min(cap).max(px(1.0))
}

fn advance_of_zero(window: &mut Window, cx: &App) -> Pixels {
    let (font_size, _) = bubble_metrics(window);
    let theme = Theme::of(cx);
    let run = TextRun {
        len: 1,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line("0".into(), font_size, &[run], None);
    let width = shaped.width();
    if width < px(1.0) { px(8.0) } else { width }
}

fn columns_for(width: Pixels, advance: Pixels) -> usize {
    let usable = (f32::from(width) - 1.0).max(f32::from(advance));
    (usable / f32::from(advance)).floor().max(1.0) as usize
}

/// Element-local y span to shape, including a little overdraw.
fn visible_span(
    bounds: Bounds<Pixels>,
    mask: ContentMask<Pixels>,
    line: f32,
) -> Option<(f32, f32)> {
    let origin_y = f32::from(bounds.origin.y);
    let height = f32::from(bounds.size.height);
    let mask_top = f32::from(mask.bounds.origin.y);
    let mask_bottom = mask_top + f32::from(mask.bounds.size.height);
    let visible_top = mask_top.max(origin_y);
    let visible_bottom = mask_bottom.min(origin_y + height);
    if visible_bottom <= visible_top {
        return None;
    }
    let overdraw = line * OVERDRAW_ROWS;
    let top = (visible_top - origin_y - overdraw).max(0.0);
    let bottom = (visible_bottom - origin_y + overdraw).min(height);
    (top < bottom).then_some((top, bottom))
}

fn content_rows(text: &str, cols: usize) -> usize {
    text.split('\n').map(|line| line_rows(line, cols)).sum()
}

fn col_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        ch if ch.is_ascii() => 1,
        _ => 2,
    }
}

fn ascii_plain(line: &str) -> bool {
    line.bytes().all(|byte| byte != b'\t' && byte < 0x80)
}

/// Visual rows of one source line. An empty line is still one row.
fn line_rows(line: &str, cols: usize) -> usize {
    let cols = cols.max(1);
    if line.is_empty() {
        return 1;
    }
    if ascii_plain(line) {
        return line.len().div_ceil(cols);
    }
    let mut rows = 0usize;
    let mut col = 0usize;
    for ch in line.chars() {
        let width = col_width(ch);
        if col > 0 && col + width > cols {
            rows += 1;
            col = 0;
        }
        col += width;
        if col >= cols {
            rows += 1;
            col = 0;
        }
    }
    if col > 0 {
        rows += 1;
    }
    rows.max(1)
}

/// Call `visit` with each visual row in `[first, last)`. Tabs expand to
/// spaces so the shaped width matches the column count.
fn visit_rows(
    line: &str,
    first: usize,
    last: usize,
    cols: usize,
    mut visit: impl FnMut(usize, &str),
) {
    if first >= last {
        return;
    }
    let cols = cols.max(1);
    if ascii_plain(line) {
        for row in first..last {
            let start = row * cols;
            if start >= line.len() {
                break;
            }
            let end = (start + cols).min(line.len());
            visit(row, &line[start..end]);
        }
        return;
    }
    let mut row = 0usize;
    let mut col = 0usize;
    let mut buf = String::new();
    for ch in line.chars() {
        let width = col_width(ch);
        if col > 0 && col + width > cols {
            if (first..last).contains(&row) {
                visit(row, &buf);
            }
            buf.clear();
            row += 1;
            col = 0;
            if row >= last {
                return;
            }
        }
        if row >= first {
            if ch == '\t' {
                buf.push_str("    ");
            } else {
                buf.push(ch);
            }
        }
        col += width;
        if col >= cols {
            if (first..last).contains(&row) {
                visit(row, &buf);
            }
            buf.clear();
            row += 1;
            col = 0;
            if row >= last {
                return;
            }
        }
    }
    if col > 0 && (first..last).contains(&row) {
        visit(row, &buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(line: &str, cols: usize) -> Vec<String> {
        let mut out = Vec::new();
        visit_rows(line, 0, usize::MAX, cols, |_, text| {
            out.push(text.to_string());
        });
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }

    #[test]
    fn ascii_rows_follow_columns() {
        assert_eq!(line_rows("", 8), 1);
        assert_eq!(line_rows("abcd", 4), 1);
        assert_eq!(line_rows("abcde", 4), 2);
        assert_eq!(rows_of("abcdefgh", 4), vec!["abcd", "efgh"]);
        assert_eq!(rows_of("abcdefgh", 4).len(), line_rows("abcdefgh", 4));
    }

    #[test]
    fn tabs_and_wide_chars_take_extra_columns() {
        assert_eq!(rows_of("\t", 4), vec!["    "]);
        assert_eq!(rows_of("\ta", 4), vec!["    ", "a"]);
        assert_eq!(rows_of("你a", 2), vec!["你", "a"]);
        assert_eq!(rows_of("\ta", 4).len(), line_rows("\ta", 4));
        assert_eq!(rows_of("你a", 2).len(), line_rows("你a", 2));
    }

    #[test]
    fn blank_lines_count() {
        assert_eq!(content_rows("a\n\nb", 80), 3);
        assert_eq!(content_rows("a\n", 80), 2);
        assert_eq!(content_rows("", 80), 1);
    }

    #[test]
    fn a_long_line_slices_the_requested_rows() {
        let line = "x".repeat(10_000);
        let mut seen = Vec::new();
        visit_rows(&line, 10, 12, 100, |row, text| {
            seen.push((row, text.to_string()))
        });
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, 10);
        assert_eq!(seen[0].1.len(), 100);
        assert_eq!(line_rows(&line, 100), 100);
    }
}
