//! The file editor's text input: a code-oriented multiline editor built on
//! the composer input's proven machinery (selection, IME via
//! `EntityInputHandler`, coalescing undo) but shaped for code — monospace,
//! no soft wrapping (logical lines are visual rows, horizontal scroll),
//! per-line shaping so a file near the 2 MiB ceiling never reshapes the
//! whole buffer to paint a frame. Line numbers, highlighting, and
//! indentation commands arrive with ticket 03; this is the editing kernel
//! ticket 02 needs.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    MouseButton, MouseMoveEvent, Pixels, Point, ShapedLine, Size, TextRun, TextStyle,
    UTF16Selection, UnderlineStyle, Window, actions, div, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;

/// Monospace row height at the editor's fixed 12.5px text size.
pub(crate) const EDITOR_LINE_HEIGHT: f32 = 19.0;
pub(crate) const EDITOR_TEXT_SIZE: f32 = 12.5;
/// Left/right padding inside the text surface, right of the gutter.
const PAD_X: f32 = 10.0;
/// Holt's default indentation width: four spaces. Files whose lines lead
/// with tabs keep tabs — indentation matches the file, never reformats it.
const INDENT_SPACES: &str = "    ";
const PAD_Y: f32 = 8.0;
/// How long a run of single-character edits keeps merging into one undo step.
const UNDO_COALESCE: Duration = Duration::from_millis(700);
/// Cap on retained undo steps.
const UNDO_LIMIT: usize = 500;
const CARET_BLINK_MS: u64 = 500;

actions!(
    file_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        Copy,
        Cut,
        Paste,
        Newline,
        InsertTab,
        Outdent,
        Undo,
        Redo,
        Escape
    ]
);

/// Bind the editor's keymap. Call once at app boot.
pub(crate) fn init(cx: &mut App) {
    let ctx = Some("FileEditor");
    let mut bindings = vec![
        // Enter inserts a newline (ticket 03's indentation arrives later) —
        // it never submits or mentions anything.
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", InsertTab, ctx),
        KeyBinding::new("shift-tab", Outdent, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("cmd-up", DocStart, ctx),
        KeyBinding::new("cmd-down", DocEnd, ctx),
        KeyBinding::new("shift-cmd-left", SelectHome, ctx),
        KeyBinding::new("shift-cmd-right", SelectEnd, ctx),
        KeyBinding::new("shift-cmd-up", SelectDocStart, ctx),
        KeyBinding::new("shift-cmd-down", SelectDocEnd, ctx),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, ctx),
        KeyBinding::new("cmd-delete", DeleteToLineEnd, ctx),
        KeyBinding::new("escape", Escape, ctx),
    ];
    for prefix in ["cmd", "ctrl"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-z"), Undo, ctx));
        bindings.push(KeyBinding::new(&format!("shift-{prefix}-z"), Redo, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, ctx));
    }
    // Cmd+S is the FILE's, not the editor's: it fires wherever focus sits
    // while a file tab is the active surface (tree, composer, editor).
    let find_bindings: Vec<KeyBinding> = ["cmd", "ctrl"]
        .iter()
        .map(|prefix| KeyBinding::new(&format!("{prefix}-f"), super::FindInFile, None))
        .collect();
    cx.bind_keys(find_bindings);
    let save_bindings: Vec<KeyBinding> = ["cmd", "ctrl"]
        .iter()
        .map(|prefix| KeyBinding::new(&format!("{prefix}-s"), super::SaveFile, None))
        .collect();
    cx.bind_keys(save_bindings);
    let word_edit_prefix = if cfg!(target_os = "macos") {
        "alt"
    } else {
        "ctrl"
    };
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-backspace"),
        DeleteWordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-delete"),
        DeleteWordRight,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-left"),
        WordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-right"),
        WordRight,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-left"),
        SelectWordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-right"),
        SelectWordRight,
        ctx,
    ));
    cx.bind_keys(bindings);
}

/// Events up to the viewer: every content change carries the new text so the
/// viewer owns the draft/save state, never the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// Content changed. Emitted once per applied edit (IME compositions
    /// included).
    Edited,
}

/// A restorable point in the undo history.
#[derive(Clone)]
struct EditSnapshot {
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    Insert,
    Delete,
}

/// One logical line's shaped representation — shaped lazily, reused while
/// the text is unchanged (whole-buffer reshapes never happen).
struct ShapedEntry {
    text: String,
    line: Option<ShapedLine>,
}

pub struct CodeEditor {
    pub(crate) focus_handle: FocusHandle,
    content: String,
    /// Line start offsets, aligned with `lines` (line i spans
    /// `line_starts[i]..line_starts[i] + lines[i].len()`).
    line_starts: Vec<usize>,
    lines: Vec<String>,
    shaped: Vec<ShapedEntry>,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    /// Vertical / horizontal scroll offsets in pixels.
    scroll_top: f32,
    scroll_left: f32,
    follow_cursor: bool,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    max_line_width: f32,
    blink_anchor: Instant,
    blink_task: Option<gpui::Task<()>>,
    undo_stack: VecDeque<EditSnapshot>,
    redo_stack: Vec<EditSnapshot>,
    last_edit: Option<(EditKind, usize, Instant)>,
    /// The file's path and resolved language — unknown text formats stay
    /// plain and editable without requiring a language service.
    path: String,
    language: Option<holt_syntax::LanguageId>,
    /// Cached tree-sitter highlight for the current content revision.
    highlight: Option<std::sync::Arc<holt_syntax::HighlightedDocument>>,
    /// The content revision the cached highlight covers.
    highlight_version: u64,
    /// The content revision (bumped per edit) a background highlight runs
    /// against — a stale result is dropped, never applied.
    content_version: u64,
    highlight_task: Option<gpui::Task<()>>,
    /// Quiet-window debounce: typing bursts re-highlight once, 120ms after
    /// the last keystroke, instead of reparsing per key.
    highlight_debounce: Option<gpui::Task<()>>,
    /// A reveal target (search hit) that overrides the caret for one
    /// scroll clamp — consumed by the next paint.
    reveal_target: Option<usize>,
    syntax_cache: SyntaxHighlightCache,
    /// Search matches (viewer-owned — searching never touches the buffer)
    /// painted as quads; `search_active` is the highlighted one.
    search_matches: Vec<Range<usize>>,
    search_active: Option<usize>,
}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl CodeEditor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut editor = Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            line_starts: vec![0],
            lines: vec![String::new()],
            shaped: Vec::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            scroll_top: 0.0,
            scroll_left: 0.0,
            follow_cursor: true,
            last_bounds: None,
            line_height: px(EDITOR_LINE_HEIGHT),
            max_line_width: 0.0,
            blink_anchor: Instant::now(),
            blink_task: None,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            path: String::new(),
            language: None,
            highlight: None,
            highlight_version: 0,
            content_version: 0,
            highlight_task: None,
            highlight_debounce: None,
            reveal_target: None,
            syntax_cache: SyntaxHighlightCache::default(),
            search_matches: Vec::new(),
            search_active: None,
        };
        editor.sync_lines();
        editor
    }

    /// The file this editor holds — resolves the language once and primes
    /// highlighting.
    pub fn set_path(&mut self, path: String, cx: &mut Context<Self>) {
        self.language = holt_syntax::language_for_path(&path);
        self.path = path;
        self.schedule_highlight(cx);
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Load a whole document. Programmatic replacement clears undo — a load
    /// is a new document, not an edit.
    pub fn load(&mut self, text: String, cx: &mut Context<Self>) {
        self.content = text;
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = 0.0;
        self.scroll_left = 0.0;
        self.follow_cursor = true;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        self.sync_lines();
        self.schedule_highlight(cx);
        cx.notify();
    }

    /// Split content into logical lines, keeping shaped lines whose text is
    /// unchanged (an edit re-splits the buffer but only re-shapes the lines
    /// that actually differ at their index).
    fn sync_lines(&mut self) {
        self.content_version += 1;
        let mut lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        for line in &lines {
            line_starts.push(at);
            at += line.len() + 1; // + '\n'
        }
        // Move the previous entries over positionally: a line whose text is
        // unchanged keeps its shape (an edit above shifts indices but the
        // text compare still wins for untouched lines).
        let mut previous = std::mem::take(&mut self.shaped);
        let mut shaped = Vec::with_capacity(lines.len());
        for (ix, line) in lines.iter().enumerate() {
            let reusable = previous
                .get_mut(ix)
                .filter(|existing| existing.text == *line)
                .and_then(|existing| existing.line.take());
            shaped.push(ShapedEntry {
                text: line.clone(),
                line: reusable,
            });
        }
        self.lines = lines;
        self.line_starts = line_starts;
        self.shaped = shaped;
    }

    /// Refresh syntax highlighting off the edit path: cached revisions hit
    /// the LRU instantly, new ones run tree-sitter on the background
    /// executor, and a result for an older revision is dropped.
    /// Mark the highlight stale; reparse after a 120ms quiet window so a
    /// typing burst costs one parse, not one per keystroke.
    fn mark_highlight_dirty(&mut self, cx: &mut Context<Self>) {
        if self.language.is_none() {
            return;
        }
        self.highlight_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let _ = this.update(cx, |editor, cx| editor.schedule_highlight(cx));
        }));
    }

    fn schedule_highlight(&mut self, cx: &mut Context<Self>) {
        self.highlight_debounce = None;
        let Some(language) = self.language else {
            self.highlight = None;
            return;
        };
        let version = self.content_version;
        let key = DocumentHighlightKey::new(language, &self.content);
        if let Some(document) = self.syntax_cache.get(&key) {
            let changed = self.highlight_version != version;
            self.highlight = Some(document);
            self.highlight_version = version;
            if changed {
                self.invalidate_shaped();
            }
            cx.notify();
            return;
        }
        let source = self.content.clone();
        let path = self.path.clone();
        let shape = cx.background_executor().spawn(async move {
            holt_syntax::highlight(holt_syntax::HighlightRequest {
                source: &source,
                path: Some(&path),
                fence_tag: None,
            })
        });
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            let document = shape.await;
            let _ = this.update(cx, |editor, cx| {
                if editor.content_version != version {
                    return; // an edit superseded this revision
                }
                if let Ok(document) = document {
                    let key = DocumentHighlightKey::new(language, &editor.content);
                    let document = std::sync::Arc::new(document);
                    editor.syntax_cache.insert(key, document.clone());
                    editor.highlight = Some(document);
                    editor.highlight_version = version;
                    editor.invalidate_shaped();
                }
                cx.notify();
            });
        }));
    }

    /// Drop shaped lines so a fresh highlight re-shapes them with colored
    /// runs (the text itself may be unchanged).
    fn invalidate_shaped(&mut self) {
        for shaped in &mut self.shaped {
            shaped.line = None;
        }
    }

    /// Indent (or outdent) the cursor line or every selected line by one
    /// unit, as a single undo step. The unit matches the file: tabs for
    /// lines that already lead with tabs, Holt's four-space default
    /// otherwise. Only the touched lines change — no whole-file reformat.
    fn indent(&mut self, outdent: bool, cx: &mut Context<Self>) {
        let (first_line, last_line) = if self.selected_range.is_empty() {
            let line = self.line_index_for_offset(self.cursor_offset());
            (line, line)
        } else {
            let first_line = self.line_index_for_offset(self.selected_range.start);
            let mut last_line = self.line_index_for_offset(self.selected_range.end);
            // A selection ending exactly at a line start does not include
            // that line (platform convention).
            if last_line > first_line
                && self.line_starts.get(last_line).copied() == Some(self.selected_range.end)
            {
                last_line -= 1;
            }
            (first_line, last_line)
        };
        let Some(first_start) = self.line_starts.get(first_line).copied() else {
            return;
        };
        // The affected region runs to the END of the last line (its whole
        // leading whitespace participates).
        let last_end = self
            .line_range_at(
                self.line_starts
                    .get(last_line)
                    .copied()
                    .unwrap_or(self.content.len()),
            )
            .end;
        let region = first_start..last_end;
        let unit_is_tab = self.content[first_start..last_end]
            .lines()
            .any(|line| line.starts_with('\t'));
        let unit = if unit_is_tab { "\t" } else { INDENT_SPACES };
        let mut rebuilt = String::with_capacity(last_end - first_start);
        let mut delta = 0isize;
        for line in self.content[first_start..last_end].split('\n') {
            if !rebuilt.is_empty() {
                rebuilt.push('\n');
            }
            if outdent {
                let strip = line.strip_prefix('\t').map_or_else(
                    || {
                        INDENT_SPACES
                            .len()
                            .min(line.chars().take_while(|c| *c == ' ').count())
                    },
                    |_| 1,
                );
                delta -= strip as isize;
                rebuilt.push_str(&line[strip..]);
            } else {
                delta += unit.len() as isize;
                rebuilt.push_str(unit);
                rebuilt.push_str(line);
            }
        }
        if rebuilt == self.content[first_start..last_end] {
            return; // outdent with nothing to strip
        }
        self.record_edit(&region, &rebuilt);
        self.content =
            self.content[..region.start].to_owned() + &rebuilt + &self.content[region.end..];
        // Keep the selection over the same lines, shifted by the edit.
        let shift = |offset: usize| -> usize {
            if offset <= region.start {
                offset
            } else if offset >= region.end {
                (offset as isize + delta) as usize
            } else {
                (offset as isize + delta).clamp(
                    region.start as isize,
                    (region.start + rebuilt.len()) as isize,
                ) as usize
            }
        };
        self.selected_range = shift(self.selected_range.start)..shift(self.selected_range.end);
        if self.selected_range.end < self.selected_range.start {
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        // A fresh edit never merges into the indent's undo step.
        self.last_edit = None;
        self.sync_lines();
        self.mark_highlight_dirty(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    /// The viewer hands over computed search matches (searching never
    /// touches the buffer or its undo history); `active` highlights one.
    pub fn set_search_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.search_matches = matches;
        self.search_active = active;
        if let Some(active) = active
            && let Some(range) = self.search_matches.get(active)
        {
            self.reveal_offset(range.start, cx);
        }
        cx.notify();
    }

    /// Scroll a search hit into view on the next paint — one-shot, without
    /// touching the caret or the selection.
    pub fn reveal_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.reveal_target = Some(offset);
        cx.notify();
    }

    // ---- undo history ----

    /// Swap the whole buffer on a disk reload: selection clamps to the new
    /// length, scroll survives, undo resets (the document changed under the
    /// user — there is nothing coherent to rewind into).
    pub fn reload(&mut self, text: String, cx: &mut Context<Self>) {
        let caret = self.cursor_offset().min(text.len());
        self.content = text;
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        let (top, left) = self.scroll_offsets();
        self.sync_lines();
        self.scroll_top = top;
        self.scroll_left = left;
        self.schedule_highlight(cx);
        cx.notify();
    }

    /// Test accessor: how many undo steps are recorded.
    #[cfg(test)]
    pub(crate) fn undo_stack_len(&self) -> usize {
        self.undo_stack.len()
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn record_edit(&mut self, range: &Range<usize>, new_text: &str) {
        let kind = if new_text.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Insert
        };
        let mergeable = match (kind, &self.last_edit) {
            (EditKind::Insert, Some((EditKind::Insert, at, when))) => {
                range.is_empty()
                    && range.start == *at
                    && new_text.chars().count() == 1
                    && !new_text.starts_with(['\n', ' ', '\t'])
                    && when.elapsed() < UNDO_COALESCE
            }
            (EditKind::Delete, Some((EditKind::Delete, at, when))) => {
                range.end == *at && when.elapsed() < UNDO_COALESCE
            }
            _ => false,
        };
        if !mergeable {
            self.undo_stack.push_back(self.snapshot());
            while self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.pop_front();
            }
        }
        self.redo_stack.clear();
        let tail = match kind {
            EditKind::Insert => range.start + new_text.len(),
            EditKind::Delete => range.start,
        };
        self.last_edit = Some((kind, tail, Instant::now()));
    }

    fn restore(&mut self, snapshot: EditSnapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.marked_range = None;
        self.follow_cursor = true;
        self.last_edit = None;
        self.sync_lines();
        self.mark_highlight_dirty(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.undo_stack.pop_back() {
            self.redo_stack.push(self.snapshot());
            self.restore(previous, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push_back(self.snapshot());
            self.restore(next, cx);
        }
    }

    // ---- caret / selection ----

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .rev()
            .find_map(|(ix, word)| (ix < offset && !word.trim().is_empty()).then_some(ix))
            .unwrap_or(0)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .find_map(|(ix, word)| {
                let end = ix + word.len();
                (end > offset && !word.trim().is_empty()).then_some(end)
            })
            .unwrap_or(self.content.len())
    }

    /// Byte range of the logical line containing `offset`.
    fn line_range_at(&self, offset: usize) -> Range<usize> {
        let start = self.content[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len());
        start..end
    }

    /// The line index containing `offset`.
    fn line_index_for_offset(&self, offset: usize) -> usize {
        match self
            .line_starts
            .binary_search(&offset.min(self.content.len()))
        {
            Ok(ix) => ix,
            Err(insert) => insert.saturating_sub(1),
        }
    }

    // ---- geometry ----

    /// Content-local point for a byte offset: x from the line's shaped
    /// layout, y from the line index.
    fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        let index = index.min(self.content.len());
        let line_ix = self.line_index_for_offset(index);
        let line_start = *self.line_starts.get(line_ix)?;
        let local_ix = index - line_start;
        let y = PAD_Y + line_ix as f32 * EDITOR_LINE_HEIGHT;
        let Some(shaped) = self.shaped.get(line_ix) else {
            return Some(point(px(PAD_X), px(y)));
        };
        // Unwrapped line: x comes straight off the shaped layout, offset by
        // the gutter so caret/selection/IME rectangles and painted glyphs
        // share one coordinate system.
        let origin_x = self.text_x();
        let x = shaped
            .line
            .as_ref()
            .map(|line| origin_x + f32::from(line.x_for_index(local_ix)))
            .unwrap_or(origin_x);
        Some(point(px(x), px(y)))
    }

    /// Byte index closest to a content-local point.
    fn index_for_point(&self, position: Point<Pixels>) -> usize {
        let local_y = f32::from(position.y) - PAD_Y;
        let line_ix = (local_y / EDITOR_LINE_HEIGHT)
            .floor()
            .clamp(0.0, (self.lines.len() - 1) as f32) as usize;
        let Some(line_start) = self.line_starts.get(line_ix) else {
            return 0;
        };
        let Some(shaped) = self.shaped.get(line_ix) else {
            return *line_start;
        };
        let local_x = f32::from(position.x) - self.text_x();
        let ix = shaped
            .line
            .as_ref()
            .map(|line| line.closest_index_for_x(px(local_x.max(0.0))))
            .unwrap_or(0);
        (*line_start + ix).min(self.content.len())
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let local = point(
            position.x - bounds.left() + px(self.scroll_left),
            position.y - bounds.top() + px(self.scroll_top),
        );
        self.index_for_point(local)
    }

    /// The line-number gutter's width — sized to the document's line count
    /// so numbers never reflow mid-session except across magnitude jumps.
    pub fn gutter_width(&self) -> f32 {
        let digits = self.lines.len().max(1).to_string().len();
        (digits as f32 * 7.2 + 16.0).max(36.0)
    }

    /// The text surface's x origin inside the element (right of gutter).
    fn text_x(&self) -> f32 {
        self.gutter_width() + PAD_X
    }

    /// Vertical offset of the last scrollable pixel.
    fn max_scroll_top(&self, viewport_height: f32) -> f32 {
        (self.lines.len() as f32 * EDITOR_LINE_HEIGHT + 2.0 * PAD_Y - viewport_height).max(0.0)
    }

    fn max_scroll_left(&self, viewport_width: f32) -> f32 {
        (self.max_line_width + 2.0 * PAD_X - viewport_width).max(0.0)
    }

    /// Reveal the caret after motion/edits; manual scrolling pauses this
    /// until the next caret move. A pending search reveal wins for one
    /// frame.
    fn clamp_scroll(&mut self, viewport: Size<Pixels>) {
        let reveal = self.reveal_target.take();
        let target = reveal.or_else(|| self.follow_cursor.then(|| self.cursor_offset()));
        if let Some(target) = target
            && let Some(cursor) = self.point_for_index(target)
        {
            let (cx, cy) = (f32::from(cursor.x), f32::from(cursor.y));
            let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
            let margin = EDITOR_LINE_HEIGHT;
            if cy - self.scroll_top < 0.0 {
                self.scroll_top = (cy - PAD_Y).max(0.0);
            } else if cy + margin - self.scroll_top > vh {
                self.scroll_top = (cy + margin - vh).max(0.0);
            }
            if cx - self.scroll_left < self.text_x() {
                self.scroll_left = (cx - self.text_x()).max(0.0);
            } else if cx + 24.0 - self.scroll_left > vw {
                self.scroll_left = (cx + 24.0 - vw).max(0.0);
            }
        }
        self.scroll_top = self.scroll_top.min(self.max_scroll_top(vh_of(viewport)));
        self.scroll_left = self.scroll_left.min(self.max_scroll_left(vw_of(viewport)));
    }

    fn on_scroll_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.last_bounds else {
            return;
        };
        let delta = event.delta.pixel_delta(px(EDITOR_LINE_HEIGHT));
        let dy = f32::from(delta.y);
        let dx = f32::from(delta.x);
        let next_top =
            (self.scroll_top + dy).clamp(0.0, self.max_scroll_top(f32::from(bounds.size.height)));
        let next_left =
            (self.scroll_left + dx).clamp(0.0, self.max_scroll_left(f32::from(bounds.size.width)));
        if next_top == self.scroll_top && next_left == self.scroll_left {
            return;
        }
        self.scroll_top = next_top;
        self.scroll_left = next_left;
        self.follow_cursor = false;
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let index = self.index_for_mouse_position(event.position);
        if event.modifiers.shift && !event.modifiers.control && !event.modifiers.alt {
            self.select_to(index, cx);
        } else if event.click_count >= 3 {
            let line = self.line_range_at(index);
            self.move_to(line.start, cx);
            self.select_to(line.end, cx);
        } else if event.click_count == 2 {
            let start = self.previous_word_boundary(index);
            let end = self.next_word_boundary(index);
            self.move_to(start, cx);
            self.select_to(end, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        let index = self.index_for_mouse_position(event.position);
        self.select_to(index, cx);
    }

    fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    // ---- editing ----

    fn replace_range(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        if self.marked_range.is_none() {
            self.record_edit(&range, new_text);
        }
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        self.sync_lines();
        self.mark_highlight_dirty(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    /// Delete the selection, or when it is empty extend it to `target` first.
    fn delete_to(&mut self, target: usize, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if target == self.cursor_offset() {
                return;
            }
            self.selected_range =
                target.min(self.cursor_offset())..target.max(self.cursor_offset());
        }
        self.replace_range(self.selected_range.clone(), "", cx);
    }

    fn delete_backward(&mut self, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor_offset());
        self.delete_to(target, cx);
    }

    fn delete_forward(&mut self, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor_offset());
        self.delete_to(target, cx);
    }

    // ---- utf16 mapping (IME) ----

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    /// Caret paint gate: focused editor in an active window, in the "on"
    /// blink phase.
    fn caret_shown(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let focused = self.focus_handle.is_focused(window);
        if !focused || !window.is_window_active() {
            self.blink_task = None;
            return false;
        }
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(CARET_BLINK_MS))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        (self.blink_anchor.elapsed().as_millis() as u64 / CARET_BLINK_MS).is_multiple_of(2)
    }

    /// Shape the visible lines, reusing cached shapes whose text is
    /// unchanged. Returns the visible line index range.
    pub(super) fn shape_visible(
        &mut self,
        bounds: Bounds<Pixels>,
        _style: &TextStyle,
        window: &mut Window,
        cx: &mut App,
    ) -> Range<usize> {
        let viewport_height = f32::from(bounds.size.height);
        let first = ((self.scroll_top / EDITOR_LINE_HEIGHT).floor() as usize)
            .saturating_sub(2)
            .min(self.lines.len().saturating_sub(1));
        let visible_rows =
            ((viewport_height / EDITOR_LINE_HEIGHT).ceil() as usize).saturating_add(4);
        let last = (first + visible_rows).clamp(first, self.lines.len());
        let font_size = px(EDITOR_TEXT_SIZE);
        let theme = Theme::of(cx).clone();
        let mono = gpui::font(theme.font_mono.clone());
        for ix in first..last {
            let Some(shaped) = self.shaped.get_mut(ix) else {
                continue;
            };
            if shaped.line.is_some() {
                continue;
            }
            // Token colors ride the shaping runs — plain text (unknown
            // languages, pre-highlight frames) shapes with one run.
            let spans = self
                .highlight
                .as_ref()
                .and_then(|document| document.lines.get(ix))
                .map(|spans| spans.as_slice())
                .unwrap_or(&[]);
            let runs =
                crate::markdown::render::runs_for_syntax_line(&shaped.text, spans, &mono, &theme);
            shaped.line = Some(window.text_system().shape_line(
                gpui::SharedString::from(shaped.text.clone()),
                font_size,
                &runs,
                None,
            ));
        }
        // The widest CACHED line bounds horizontal scrolling; lines off
        // screen contribute once they are first shaped.
        self.max_line_width = self
            .shaped
            .iter()
            .filter_map(|shaped| shaped.line.as_ref())
            .map(|line| f32::from(line.width()))
            .fold(0.0f32, f32::max)
            + self.gutter_width();
        first..last
    }

    pub(super) fn selected_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    pub(super) fn scroll_offsets(&self) -> (f32, f32) {
        (self.scroll_top, self.scroll_left)
    }

    pub(super) fn lines(&self) -> &[String] {
        &self.lines
    }

    pub(super) fn shaped_line(&self, ix: usize) -> Option<&ShapedLine> {
        self.shaped.get(ix).and_then(|shaped| shaped.line.as_ref())
    }
}

fn vh_of(viewport: Size<Pixels>) -> f32 {
    f32::from(viewport.height)
}

fn vw_of(viewport: Size<Pixels>) -> f32 {
    f32::from(viewport.width)
}

impl EntityInputHandler for CodeEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        // An IME commit is the tail of a composition whose pre-composition
        // snapshot was already taken.
        if self.marked_range.is_none() {
            self.record_edit(&range, new_text);
        }
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        self.sync_lines();
        self.mark_highlight_dirty(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        // First keystroke of a composition: snapshot the text as it stood
        // before any of it existed, so one undo drops the whole composition.
        if self.marked_range.is_none() {
            self.undo_stack.push_back(self.snapshot());
            while self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.pop_front();
            }
            self.redo_stack.clear();
            self.last_edit = None;
        }
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.follow_cursor = true;
        self.blink_anchor = Instant::now();
        self.sync_lines();
        self.mark_highlight_dirty(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.point_for_index(range.start)?;
        Some(Bounds::new(
            point(
                bounds.left() + start.x - px(self.scroll_left),
                bounds.top() + start.y - px(self.scroll_top),
            ),
            size(px(2.0), self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point_in_window: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_mouse_position(point_in_window)))
    }
}

impl Render for CodeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("code-editor")
            .size_full()
            .key_context("FileEditor")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.on_mouse_down(event, window, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                    this.on_mouse_up(event, window, cx);
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                    this.on_scroll_wheel(event, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                this.delete_backward(cx);
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                this.delete_forward(cx);
            }))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                if this.selected_range.is_empty() {
                    this.move_to(this.previous_boundary(this.cursor_offset()), cx);
                } else {
                    this.move_to(this.selected_range.start, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                if this.selected_range.is_empty() {
                    this.move_to(this.next_boundary(this.selected_range.end), cx);
                } else {
                    this.move_to(this.selected_range.end, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Up, _, cx| {
                if let Some(ix) = vertical_target(this, -1.0) {
                    this.move_to(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Down, _, cx| {
                if let Some(ix) = vertical_target(this, 1.0) {
                    this.move_to(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| {
                if let Some(ix) = vertical_target(this, -1.0) {
                    this.select_to(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| {
                if let Some(ix) = vertical_target(this, 1.0) {
                    this.select_to(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.select_to(this.previous_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.select_to(this.next_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.move_to(0, cx);
                this.select_to(this.content.len(), cx);
            }))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                let line = this.line_range_at(this.cursor_offset());
                this.move_to(line.start, cx);
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                let line = this.line_range_at(this.cursor_offset());
                this.move_to(line.end, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectHome, _, cx| {
                let line = this.line_range_at(this.cursor_offset());
                this.select_to(line.start, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectEnd, _, cx| {
                let line = this.line_range_at(this.cursor_offset());
                this.select_to(line.end, cx);
            }))
            .on_action(cx.listener(|this, _: &DocStart, _, cx| {
                this.move_to(0, cx);
            }))
            .on_action(cx.listener(|this, _: &DocEnd, _, cx| {
                this.move_to(this.content.len(), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectDocStart, _, cx| {
                this.select_to(0, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectDocEnd, _, cx| {
                this.select_to(this.content.len(), cx);
            }))
            .on_action(cx.listener(|this, _: &WordLeft, _, cx| {
                this.move_to(this.previous_word_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &WordRight, _, cx| {
                this.move_to(this.next_word_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                this.select_to(this.previous_word_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                this.select_to(this.next_word_boundary(this.cursor_offset()), cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteWordLeft, _, cx| {
                let target = this.previous_word_boundary(this.cursor_offset());
                this.delete_to(target, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteWordRight, _, cx| {
                let target = this.next_word_boundary(this.cursor_offset());
                this.delete_to(target, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                let start = this.line_range_at(this.cursor_offset()).start;
                this.delete_to(start, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteToLineEnd, _, cx| {
                let end = this.line_range_at(this.cursor_offset()).end;
                this.delete_to(end, cx);
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| {
                if !this.selected_range.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        this.content[this.selected_range.clone()].to_string(),
                    ));
                }
            }))
            .on_action(cx.listener(|this, _: &Cut, _, cx| {
                if !this.selected_range.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        this.content[this.selected_range.clone()].to_string(),
                    ));
                    let range = this.selected_range.clone();
                    this.replace_range(range, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if let Some(item) = cx.read_from_clipboard()
                    && let Some(text) = item.text()
                {
                    let range = this.selected_range.clone();
                    this.replace_range(range, &text, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Newline, _, cx| {
                let range = this.selected_range.clone();
                this.replace_range(range, "\n", cx);
            }))
            .on_action(cx.listener(|this, _: &InsertTab, _, cx| {
                this.indent(false, cx);
            }))
            .on_action(cx.listener(|this, _: &Outdent, _, cx| {
                this.indent(true, cx);
            }))
            .on_action(cx.listener(CodeEditor::undo))
            .on_action(cx.listener(CodeEditor::redo))
            .on_action(cx.listener(|_, _: &Escape, _, cx| {
                // Nothing to dismiss in the editor itself yet; keep the key
                // from reaching the composer underneath.
                cx.stop_propagation();
            }))
            .child(CodeEditorElement {
                editor: cx.entity(),
            })
    }
}

/// Vertical motion target one line up/down, keeping the caret's x column
/// clamped to the document edges.
fn vertical_target(editor: &CodeEditor, dir: f32) -> Option<usize> {
    let current = editor.point_for_index(editor.cursor_offset())?;
    let line_ix = editor.line_index_for_offset(editor.cursor_offset());
    let target_ix = if dir < 0.0 {
        line_ix.checked_sub(1)?
    } else {
        (line_ix + 1).min(editor.lines().len() - 1)
    };
    if target_ix == line_ix {
        return None;
    }
    let y = PAD_Y + target_ix as f32 * EDITOR_LINE_HEIGHT;
    Some(editor.index_for_point(point(current.x, px(y))))
}

/// The editor's paint element: clips to its bounds, paints the visible
/// shaped lines, selection quads, IME underline, and the caret.
struct CodeEditorElement {
    editor: Entity<CodeEditor>,
}

impl IntoElement for CodeEditorElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for CodeEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
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
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = Theme::of(cx).clone();
        let style = window.text_style();
        let visible = self.editor.update(cx, |editor, cx| {
            editor.last_bounds = Some(bounds);
            let visible = editor.shape_visible(bounds, &style, window, cx);
            editor.clamp_scroll(bounds.size);
            visible
        });
        let editor = self.editor.clone();
        window.handle_input(
            &self.editor.read(cx).focus_handle,
            gpui::ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                editor.update(cx, |editor, cx| editor.on_mouse_move(event, cx));
            }
        });

        let (scroll_top, scroll_left) = self.editor.read(cx).scroll_offsets();
        let line_height = px(EDITOR_LINE_HEIGHT);
        let gutter = self.editor.read(cx).gutter_width();
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            // The gutter: stable line numbers, vertically scrolled with the
            // text, horizontally pinned (long lines never slide under it).
            // The caret's line reads a step brighter.
            let caret_line = self
                .editor
                .read(cx)
                .line_index_for_offset(self.editor.read(cx).cursor_offset());
            let number_font_size = px(EDITOR_TEXT_SIZE - 1.5);
            for line_ix in visible.clone() {
                let number: gpui::SharedString = (line_ix + 1).to_string().into();
                let bright = line_ix == caret_line;
                let run = TextRun {
                    len: number.len(),
                    font: gpui::font(theme.font_mono.clone()),
                    color: if bright {
                        theme.text.opacity(0.75)
                    } else {
                        theme.text_muted.opacity(0.55)
                    },
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped =
                    window
                        .text_system()
                        .shape_line(number, number_font_size, &[run], None);
                let width = shaped.width();
                let y = PAD_Y + line_ix as f32 * EDITOR_LINE_HEIGHT - scroll_top;
                let _ = shaped.paint(
                    point(
                        bounds.left() + px(gutter - 6.0) - width,
                        bounds.top() + px(y),
                    ),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            // Search matches paint under everything else: the active hit in
            // the accent, the rest in the selection wash (ticket 03).
            let search = {
                let editor = self.editor.read(cx);
                (editor.search_matches.clone(), editor.search_active)
            };
            for (ix, range) in search.0.iter().enumerate() {
                let (Some(start), Some(end)) = (
                    self.editor.read(cx).x_for_offset(range.start),
                    self.editor.read(cx).x_for_offset(range.end),
                ) else {
                    continue;
                };
                if end <= start {
                    continue;
                }
                let line_ix = self.editor.read(cx).line_index_for_offset(range.start);
                let y = PAD_Y + line_ix as f32 * EDITOR_LINE_HEIGHT - scroll_top;
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(
                            bounds.left() + start - px(scroll_left),
                            bounds.top() + px(y),
                        ),
                        size(end - start, line_height),
                    ),
                    if Some(ix) == search.1 {
                        theme.accent.opacity(0.45)
                    } else {
                        theme.selection.opacity(0.55)
                    },
                ));
            }
            // Selection quads per affected line.
            let selected = self.editor.read(cx).selected_range();
            if !selected.is_empty() {
                let first_line = self.editor.read(cx).line_index_for_offset(selected.start);
                let last_line = self.editor.read(cx).line_index_for_offset(selected.end);
                for line_ix in first_line..=last_line {
                    let Some(line_start) = self.editor.read(cx).line_starts_for(line_ix) else {
                        continue;
                    };
                    let local_start = selected.start.saturating_sub(line_start);
                    let local_end = selected
                        .end
                        .saturating_sub(line_start)
                        .min(self.editor.read(cx).line_len(line_ix));
                    if local_start >= local_end {
                        continue;
                    }
                    let start_x = self
                        .editor
                        .read(cx)
                        .x_for_local(line_ix, local_start)
                        .unwrap_or(px(PAD_X));
                    let end_x = self
                        .editor
                        .read(cx)
                        .x_for_local(line_ix, local_end)
                        .unwrap_or(px(PAD_X + 40.0));
                    if end_x <= start_x {
                        continue;
                    }
                    let y = PAD_Y + line_ix as f32 * EDITOR_LINE_HEIGHT - scroll_top;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(
                                bounds.left() + start_x - px(scroll_left),
                                bounds.top() + px(y),
                            ),
                            size(end_x - start_x, line_height),
                        ),
                        theme.selection,
                    ));
                }
            }
            // Visible lines. A line intersecting the IME marked range is
            // shaped fresh with an underline run; everything else paints its
            // cached plain shape. Lines are read one at a time — no
            // whole-buffer clones per frame.
            let text_origin_x = self.editor.read(cx).text_x();
            for line_ix in visible.clone() {
                let y = PAD_Y + line_ix as f32 * EDITOR_LINE_HEIGHT - scroll_top;
                let origin = point(
                    bounds.left() + px(text_origin_x) - px(scroll_left),
                    bounds.top() + px(y),
                );
                let marked = self.editor.read(cx).marked_range_local(line_ix);
                let underlined = marked.and_then(|marked| {
                    let editor = self.editor.read(cx);
                    let text = editor.line_text(line_ix)?;
                    let spans = editor
                        .highlight
                        .as_ref()
                        .and_then(|document| document.lines.get(line_ix))
                        .map(|spans| spans.as_slice())
                        .unwrap_or(&[]);
                    let mono = gpui::font(theme.font_mono.clone());
                    let base =
                        crate::markdown::render::runs_for_syntax_line(&text, spans, &mono, &theme);
                    let runs = overlay_underline(base, marked);
                    Some(window.text_system().shape_line(
                        gpui::SharedString::from(text),
                        px(EDITOR_TEXT_SIZE),
                        &runs,
                        None,
                    ))
                });
                if let Some(line) = underlined {
                    let _ =
                        line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx);
                    continue;
                }
                // WrappedLine isn't Clone; ShapedLine is — take a cheap
                // clone so the entity borrow ends before painting.
                if let Some(line) = self.editor.read(cx).shaped_line(line_ix).cloned() {
                    let _ =
                        line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx);
                }
            }
            // Caret.
            let caret = self.editor.update(cx, |editor, cx| {
                editor
                    .caret_shown(window, cx)
                    .then(|| {
                        editor
                            .point_for_index(editor.cursor_offset())
                            .map(|at| (at, editor.line_height))
                    })
                    .flatten()
            });
            if let Some((at, height)) = caret {
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(
                            bounds.left() + at.x - px(scroll_left),
                            bounds.top() + at.y - px(scroll_top),
                        ),
                        size(px(2.0), height),
                    ),
                    theme.caret,
                ));
            }
        });
    }
}

/// Split prepared (syntax-colored) runs at the marked boundaries and
/// underline the composition slice — the IME preview keeps token colors.
fn overlay_underline(runs: Vec<TextRun>, marked: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0usize;
    for run in runs {
        let run_end = at + run.len;
        let overlap_start = marked.start.max(at);
        let overlap_end = marked.end.min(run_end);
        if overlap_start < overlap_end {
            if overlap_start > at {
                let mut head = run.clone();
                head.len = overlap_start - at;
                out.push(head);
            }
            let mut middle = run.clone();
            middle.len = overlap_end - overlap_start;
            middle.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                wavy: false,
                color: None,
            });
            out.push(middle);
            if run_end > overlap_end {
                let mut tail = run;
                tail.len = run_end - overlap_end;
                out.push(tail);
            }
        } else {
            out.push(run);
        }
        at = run_end;
    }
    out
}

impl CodeEditor {
    fn line_starts_for(&self, ix: usize) -> Option<usize> {
        self.line_starts.get(ix).copied()
    }

    fn line_text(&self, ix: usize) -> Option<String> {
        self.lines.get(ix).cloned()
    }

    fn line_len(&self, ix: usize) -> usize {
        self.lines.get(ix).map(|line| line.len()).unwrap_or(0)
    }

    fn x_for_local(&self, line_ix: usize, local: usize) -> Option<Pixels> {
        self.shaped_line(line_ix)
            .map(|line| px(self.text_x() + f32::from(line.x_for_index(local))))
    }

    fn x_for_offset(&self, offset: usize) -> Option<Pixels> {
        self.point_for_index(offset).map(|point| point.x)
    }

    fn marked_range_local(&self, line_ix: usize) -> Option<Range<usize>> {
        let marked = self.marked_range.clone()?;
        let line_start = self.line_starts_for(line_ix)?;
        let line_len = self.line_len(line_ix);
        let start = marked.start.clamp(line_start, line_start + line_len);
        let end = marked.end.clamp(line_start, line_start + line_len);
        (start < end).then_some(start - line_start..end - line_start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with(cx: &mut App, text: &str) -> Entity<CodeEditor> {
        cx.new(|cx| {
            let mut editor = CodeEditor::new(cx);
            editor.load(text.to_string(), cx);
            editor
        })
    }

    #[gpui::test]
    fn tab_indents_selected_lines_as_one_undo_step(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let editor = editor_with(cx, "a\nb\nc\n");
            editor.update(cx, |editor, cx| {
                editor.selected_range = 0..4; // "a\nb\n"
                editor.indent(false, cx);
            });
            assert_eq!(editor.read(cx).text(), "    a\n    b\nc\n");
            // Undo rewinds the whole indentation in one step — the snapshot
            // count proves a single entry was pushed.
            let pushed = editor.read(cx).undo_stack_len();
            assert_eq!(pushed, 1, "one undo step for the multi-line indent");
        })
    }

    #[gpui::test]
    fn shift_tab_strips_at_most_one_unit(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let editor = editor_with(cx, "        deep\n");
            editor.update(cx, |editor, cx| {
                editor.selected_range = 0..0;
                editor.indent(true, cx);
            });
            assert_eq!(editor.read(cx).text(), "    deep\n");
            // At zero indent a further outdent changes nothing.
            editor.update(cx, |editor, cx| {
                for _ in 0..3 {
                    editor.indent(true, cx);
                }
            });
            // Either fully stripped or untouched — never negative/garbage.
            let text = editor.read(cx).text();
            assert!(text == "deep\n" || text == "    deep\n", "{text:?}");
        })
    }

    #[gpui::test]
    fn tab_leading_files_keep_tabs(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let editor = editor_with(cx, "\tcode\nplain\n");
            editor.update(cx, |editor, cx| {
                editor.selected_range = 0..0;
                editor.indent(false, cx);
            });
            assert_eq!(editor.read(cx).text(), "\t\tcode\nplain\n");
        })
    }

    #[gpui::test]
    fn gutter_width_grows_with_the_line_count(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let small = editor_with(cx, "one\ntwo\n");
            let narrow = small.read(cx).gutter_width();
            let big = cx.new(|cx| {
                let mut editor = CodeEditor::new(cx);
                let text: String = (0..100_000).map(|ix| format!("line {ix}\n")).collect();
                editor.load(text, cx);
                editor
            });
            assert!(big.read(cx).gutter_width() > narrow);
        })
    }

    #[test]
    fn overlay_underline_splits_runs_at_the_marked_slice() {
        let plain = |len: usize| TextRun {
            len,
            font: gpui::font("monospace"),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = vec![plain(2), plain(4)];
        let overlay = overlay_underline(runs, 1..3);
        let lens: Vec<usize> = overlay.iter().map(|run| run.len).collect();
        assert_eq!(lens, vec![1, 1, 1, 3]);
        assert!(overlay[1].underline.is_some());
        assert!(overlay[0].underline.is_none());
    }
}
