//! The right-pane "Changes" content (feature-inventory §1.11): a unified-diff
//! viewer over `WatchCheckoutDiffs`.
//!
//! - pure patch parser: `diff --git` sections → file/hunk/line/notice rows,
//!   with add/delete/rename/binary detection and per-file counts;
//! - resolution: the shown diff matches the selected chat by `checkout_id`
//!   first, then by device+cwd, then cwd alone;
//! - states: *preparing* (no diff yet), *clean* (empty patch), *list*; a watch
//!   error shows a banner while the last content stays;
//! - virtualized with gpui `list()` at LINE granularity — every file header,
//!   hunk header, and diff line is its own row (the flat model Zed's editor
//!   uses for its project diff: only the visible slice materializes, and a
//!   collapsed file's body rows are removed from the list outright, not
//!   hidden); each section collapses with a 180 ms height tween on a
//!   clipped stand-in row (analytic heights, capped to what the clip can
//!   reveal) and a 200 ms chevron transition;
//! - syntax highlight reuses the markdown tokenizer per diff line, computed
//!   time-sliced on the background executor and applied as paint-only run
//!   colors (layout never changes);
//! - scopes (t3code parity): *Working tree* rides the watch stream; *Branch
//!   changes* (vs a selectable base ref, default branch preselected) and
//!   *Latest turn* fetch one-shot `GetCheckoutDiff` captures, refreshed when
//!   the watch checksum says the tree moved;
//! - two layouts ([`DiffMode`], toolbar toggle, persisted): *unified* stacks
//!   old and new; *split* pairs each hunk's deletions against its additions
//!   into one row with two columns. Split is a pure re-flatten of the same
//!   parse — the row model, virtualization, folds, and highlights are shared.
//!   Its left column is inert: notes are cited against the post-change file,
//!   so only the right column takes a `+` (already-staged old-side notes
//!   still show their cards).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Context, Entity, FocusHandle, ListAlignment, ListState, SharedString, Subscription, Task, px,
};

use holt_proto::{CheckoutDiff, GitHistoryCommit};

use crate::comments::CommentSide;
use crate::composer::ComposerInput;
use crate::history::{GitHistory, GitHistoryCount, GitHistoryFetchButton};
use crate::popover::Popup;
use crate::state::AppState;

mod comments;
mod model;
mod render;
mod rows;
mod sync;

pub use model::{
    DiffHighlights, DiffLine, DiffPhase, DiffScope, FileDiff, FileStatus, Hunk, LineKind, LinePair,
    SourceLineRef, SourceSide, apply_diff_frame, clean_message, default_base_ref, diff_phase,
    file_notices, gutter_width, line_anchor, parse_patch, resolve_diff, scope_label, split_pairs,
    split_pairs_upto, truncate_file_lines, uncommitted_label,
};

pub(crate) use render::render_file_body_with_syntax;
pub use render::{COMMENT_ADDER_SIZE, comment_adder_left, split_adder_left};

pub use rows::{DiffRow, body_height, body_height_with, body_row_count, body_rows, flatten_rows};

// ---------------------------------------------------------------------------
// Layout numbers (analytic — they drive the fold tween)
// ---------------------------------------------------------------------------

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
const STICKY_FILE_HEADER_BLUR: f32 = 16.0;
/// Coverage of the theme's content-plane tint over the sticky header blur.
/// Light needs substantially more coverage: dark text is much more vulnerable
/// to rows ghosting through the blur than light text is on a dark tint.
const STICKY_FILE_HEADER_TINT_ALPHA_DARK: f32 = 0.40;
const STICKY_FILE_HEADER_TINT_ALPHA_LIGHT: f32 = 0.85;
pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 21.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const BODY_BOTTOM_PAD: f32 = 8.0;
/// Gutter width per line-number column.
pub const GUTTER_WIDTH: f32 = 36.0;
/// The +/−/· marker column between the gutters and the code.
pub const MARKER_WIDTH: f32 = 28.0;
/// Width of the coloured accent bar on the left edge of +/− rows.
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
/// The marker column in split mode: each half pays for its own, so it is
/// narrower than [`MARKER_WIDTH`] to leave the code the room.
pub const SPLIT_MARKER_WIDTH: f32 = 18.0;
/// Hairline between the two split columns.
pub const SPLIT_DIVIDER_WIDTH: f32 = 1.0;
const DIFF_TEXT_SIZE: f32 = 12.0;

/// How the diff is laid out. Persisted in `ui-settings.json` (`diffSplit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffMode {
    /// One column: deletions above additions (the classic patch reading).
    #[default]
    Unified,
    /// Two columns: old on the left, new on the right, paired per hunk.
    Split,
}

impl DiffMode {
    pub fn from_split(split: bool) -> Self {
        if split { Self::Split } else { Self::Unified }
    }

    pub fn is_split(self) -> bool {
        self == Self::Split
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Unified => Self::Split,
            Self::Split => Self::Unified,
        }
    }
}

/// Read-modify-write `ui-settings.json` for just the split-diff key — a fresh
/// load, for the reason [`crate::appearance`] documents: the shell holds its
/// own `UiSettings` and saves it debounced, so writing a cached snapshot from
/// here would roll back a pane resize made seconds earlier.
fn persist_split(split: bool, data_dir: &std::path::Path) {
    let mut settings = crate::settings::UiSettings::load(data_dir);
    settings.diff_split = split;
    if let Err(err) = settings.save(data_dir) {
        tracing::warn!(error = %err, "could not persist diff layout");
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

struct ParsedDiff {
    /// `checkout_id:checksum` — identity of the parsed content.
    key: String,
    truncated: bool,
    additions: u32,
    deletions: u32,
    file_count: usize,
    files: Arc<Vec<FileDiff>>,
}

#[derive(Default, Clone, Copy)]
struct FileFold {
    collapsed: bool,
    /// Bumped per toggle — keys the height tween + chevron transition.
    epoch: usize,
    from: f32,
    to: f32,
    /// When the toggle happened: the tweens are armed only briefly after the
    /// click — gpui replays an element's animation on remount, and in the
    /// virtualized list a row scrolling back into view is a remount (the
    /// transcript's tool groups had the same flash; user report).
    toggled_at: Option<std::time::Instant>,
}

/// Tween arming window after a fold toggle (COLLAPSE's 180ms plus margin).
const FOLD_TWEEN_WINDOW: Duration = Duration::from_millis(400);

/// Ceiling on how much body a fold tween's stand-in row materializes. A
/// tween always starts from a clicked (on-screen) header, so the revealable
/// slice is at most one viewport tall — everything past this is clipped or
/// below the fold either way.
const FOLD_TWEEN_MAX_PX: f32 = 2400.0;

impl FileFold {
    fn animating(&self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW)
    }
}

struct HighlightSlot {
    fingerprint: u64,
    state: DiffHighlightState,
    _excerpt_task: Option<Task<()>>,
    _fetch_task: Option<Task<()>>,
}

enum DiffHighlightState {
    Pending,
    Ready(Arc<DiffHighlights>),
    Excerpt(Arc<DiffHighlights>),
    Plain,
}

/// The open base-ref dropdown — the same searchable-menu recipe as the
/// composer's ref picker and the spaces filter: a filter input on top
/// (`PaletteSearch` context so ↑↓/⏎ bubble to the card's key handler),
/// ranked substring rows below.
struct RefMenu {
    search: Entity<ComposerInput>,
    /// Keyboard highlight within the filtered rows.
    active: usize,
    /// Tracked on the card — puts it on the keyboard dispatch path while the
    /// search input holds focus (the structure every working picker uses).
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

/// The line the pointer is on. Only one element per anchor ever takes the
/// hover — the unified row, or a split row's right column — so the anchor
/// alone identifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverRow {
    path: String,
    side: CommentSide,
    line: u32,
}

struct CommentDraft {
    /// Composer the note will stage onto, captured when the card opened. A
    /// draft belongs to the checkout it was written over, so it must not
    /// follow the user onto whatever chat is selected by commit time.
    key: String,
    path: String,
    /// The file's pre-rename path, when it moved — carried onto the comment so
    /// an `Old`-side citation names the file that line lives in.
    old_path: Option<String>,
    side: CommentSide,
    line: u32,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// The Changes pane entity. Lazy: no RPC until [`Changes::ensure_watch`] runs
/// (the shell calls it when the pane first opens).
pub struct Changes {
    state: Entity<AppState>,
    diffs: Vec<CheckoutDiff>,
    started: bool,
    error: Option<SharedString>,
    watch_task: Option<Task<()>>,
    parsed: Option<ParsedDiff>,
    parse_task: Option<Task<()>>,
    folds: HashMap<String, FileFold>,
    highlights: HashMap<String, HighlightSlot>,
    /// The flattened row model the list virtualizes over (line granularity;
    /// collapsed bodies excluded) + each file's row span within it.
    rows: Vec<DiffRow>,
    row_ranges: Vec<std::ops::Range<usize>>,
    /// Sweeps [`DiffRow::FoldingBody`] stand-ins back to steady-state rows
    /// once their tween window elapses.
    fold_settle: Option<Task<()>>,
    list: ListState,
    /// What the pane diffs against (toolbar dropdown).
    scope: DiffScope,
    /// Unified or side-by-side (toolbar toggle, persisted per user).
    mode: DiffMode,
    /// Comparison ref for [`DiffScope::Branch`] — preset to the repo's
    /// default branch once the branch list lands.
    base_ref: Option<String>,
    branches: Vec<String>,
    /// The cwd the branch list was fetched for.
    branches_for: Option<String>,
    branches_task: Option<Task<()>>,
    /// One-shot scoped capture (Branch / Latest turn) + its fetch key.
    scoped: Option<CheckoutDiff>,
    scoped_for: Option<String>,
    scoped_error: Option<SharedString>,
    scoped_inflight: Option<String>,
    scoped_task: Option<Task<()>>,
    scope_menu: Popup<()>,
    ref_menu: Popup<RefMenu>,
    /// Only ever one: a second `+` moves the card rather than stacking two
    /// half-written notes.
    draft: Option<CommentDraft>,
    hover: Option<HoverRow>,
    comment_key: u64,
    history: Option<Entity<GitHistory>>,
    history_count: Option<Entity<GitHistoryCount>>,
    history_fetch_button: Option<Entity<GitHistoryFetchButton>>,
    history_events: Option<Subscription>,
    /// Pinned commit for a [`DiffScope::Commit`] pane (sha + subject drive
    /// the fetch and the surface-tab title).
    commit: Option<GitHistoryCommit>,
    _observe: Subscription,
}

/// Events the host (the right pane's surface strip) listens for.
pub enum ChangesEvent {
    /// A History row was clicked — open this commit as its own diff tab.
    OpenCommit(GitHistoryCommit),
}

impl gpui::EventEmitter<ChangesEvent> for Changes {}

impl Changes {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let mode = DiffMode::from_split(
            state
                .read(cx)
                .data_dir
                .as_deref()
                .is_some_and(|dir| crate::settings::UiSettings::load(dir).diff_split),
        );
        Self {
            state,
            mode,
            diffs: Vec::new(),
            started: false,
            error: None,
            watch_task: None,
            parsed: None,
            parse_task: None,
            folds: HashMap::new(),
            highlights: HashMap::new(),
            rows: Vec::new(),
            row_ranges: Vec::new(),
            fold_settle: None,
            // Rows are single lines now — a deep overdraw is cheap and keeps
            // fast wheel flicks from outrunning measurement.
            list: ListState::new(0, ListAlignment::Top, px(1024.0)),
            scope: DiffScope::default(),
            base_ref: None,
            branches: Vec::new(),
            branches_for: None,
            branches_task: None,
            scoped: None,
            scoped_for: None,
            scoped_error: None,
            scoped_inflight: None,
            scoped_task: None,
            scope_menu: Popup::default(),
            ref_menu: Popup::default(),
            draft: None,
            hover: None,
            comment_key: 0,
            history: None,
            history_count: None,
            history_fetch_button: None,
            history_events: None,
            commit: None,
            _observe: observe,
        }
    }

    /// A pane pinned to one commit's diff (a History row click) — fetches
    /// `parent vs commit` once and never offers the scope menu.
    pub fn for_commit(
        state: Entity<AppState>,
        commit: GitHistoryCommit,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut changes = Self::new(state, cx);
        changes.scope = DiffScope::Commit;
        changes.commit = Some(commit);
        changes
    }

    /// The surface-tab title (contextual, user request): the pinned commit's
    /// subject (short sha for subject-less commits), else the scope's label.
    pub fn tab_title(&self) -> gpui::SharedString {
        if let Some(commit) = &self.commit {
            let subject = commit.subject.trim();
            if !subject.is_empty() {
                return subject.to_string().into();
            }
            return commit.sha.chars().take(7).collect::<String>().into();
        }
        gpui::SharedString::from(self.scope.label())
    }
}
