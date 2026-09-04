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
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, ListAlignment, ListState,
    SharedString, Subscription, Task, Window, div, font, list, prelude::*, px,
};

use holt_proto::{CheckoutDiff, GitHistoryCommit};
use holt_rpc::methods;

use crate::comments::{self, CommentSide, DiffComment};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::history::{GitHistory, GitHistoryCount, GitHistoryEvent, GitHistoryFetchButton};
use crate::markdown::render;
use crate::motion::{self, AnimationExt as _, CHEVRON, COLLAPSE};
use crate::popover::{self, Popup};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

mod model;
mod rows;
mod sync;

pub use model::{
    DiffHighlights, DiffLine, DiffPhase, DiffScope, FileDiff, FileStatus, Hunk, LineKind, LinePair,
    SourceLineRef, SourceSide, apply_diff_frame, clean_message, default_base_ref, diff_phase,
    file_notices, gutter_width, line_anchor, parse_patch, resolve_diff, scope_label, split_pairs,
    split_pairs_upto, truncate_file_lines, uncommitted_label,
};
use model::{comment_state_key, excerpt_highlights, full_highlights, hash64};

pub use rows::{DiffRow, body_height, body_height_with, body_row_count, body_rows, flatten_rows};
use rows::{
    FileHeaderPresentation, sticky_file_header, sticky_file_header_paint, sticky_header_push_offset,
};

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

    /// Start the `WatchCheckoutDiffs` subscription (idempotent). Retries with
    /// a flat 2 s delay if the stream fails or ends; the last content stays
    /// visible under an error banner meanwhile.
    pub fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            // Engine still booting — retry on the next state change via sync().
            return;
        };
        self.started = true;
        self.watch_task = Some(Self::spawn_watch(engine, cx));
    }

    fn spawn_watch(engine: EngineHandle, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let subscribed = engine
                    .client()
                    .subscribe(methods::WATCH_CHECKOUT_DIFFS, serde_json::json!({}))
                    .await;
                match subscribed {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            let alive = this.update(cx, |changes, cx| {
                                changes.error = None;
                                if apply_diff_frame(&mut changes.diffs, value) {
                                    changes.sync(cx);
                                    cx.notify();
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        // Stream ended (engine restart / reconnect): banner + retry.
                        if this
                            .update(cx, |changes, cx| {
                                changes.error = Some("Diff stream interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        if this
                            .update(cx, |changes, cx| {
                                changes.error =
                                    Some(format!("Diff watch unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    fn resolved(&self, cx: &App) -> Option<CheckoutDiff> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        resolve_diff(&self.diffs, chat).cloned()
    }

    /// The checkout root the scoped RPCs address: the watch-resolved diff's
    /// canonical cwd when available, else the chat row's own.
    fn scoped_cwd(&self, cx: &App) -> Option<String> {
        if let Some(diff) = self.resolved(cx) {
            return Some(diff.cwd);
        }
        self.state.read(cx).selected_chat_row()?.cwd.clone()
    }

    /// The diff the pane currently displays: the watch stream for the working
    /// tree, the one-shot scoped capture otherwise.
    fn active_diff(&self, cx: &App) -> Option<CheckoutDiff> {
        match self.scope {
            DiffScope::WorkingTree => self.resolved(cx),
            DiffScope::Branch | DiffScope::LatestTurn | DiffScope::Commit => self.scoped.clone(),
            DiffScope::History => None,
        }
    }

    /// Scope discriminant folded into the parse key, so a scope or base
    /// switch re-parses even when checksums collide.
    fn scope_key(&self) -> String {
        match self.scope {
            DiffScope::WorkingTree => "wt".to_string(),
            DiffScope::Branch => format!("br:{}", self.base_ref.as_deref().unwrap_or("")),
            DiffScope::LatestTurn => "turn".to_string(),
            DiffScope::History => "history".to_string(),
            DiffScope::Commit => format!(
                "commit:{}",
                self.commit.as_ref().map(|c| c.sha.as_str()).unwrap_or("")
            ),
        }
    }

    fn parse_key(&self, diff: &CheckoutDiff) -> String {
        format!(
            "{}:{}:{}",
            diff.checkout_id,
            diff.checksum,
            self.scope_key()
        )
    }

    /// Fetch the branch list for the selected chat's checkout (idempotent per
    /// cwd); the repo's default branch (first entry) becomes the
    /// comparison base unless the user already picked one that still exists.
    fn ensure_branches(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.scoped_cwd(cx) else {
            return;
        };
        let key = cwd.clone();
        if self.branches_for.as_deref() == Some(key.as_str()) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.branches_for = Some(key.clone());
        self.branches_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd));
            let result = engine
                .client()
                .call(methods::LIST_BRANCHES, serde_json::Value::Object(params))
                .await;
            this.update(cx, |changes, cx| {
                if changes.branches_for.as_deref() != Some(key.as_str()) {
                    return; // superseded by a chat switch
                }
                match result {
                    Ok(value) => {
                        changes.branches =
                            serde_json::from_value::<Vec<String>>(value).unwrap_or_default();
                        let keep = changes
                            .base_ref
                            .as_ref()
                            .is_some_and(|base| changes.branches.contains(base));
                        if !keep {
                            let current = changes
                                .state
                                .read(cx)
                                .selected_chat_row()
                                .and_then(|chat| chat.branch.clone());
                            changes.base_ref =
                                default_base_ref(&changes.branches, current.as_deref());
                        }
                        changes.sync(cx);
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "changes: branch list failed");
                        // Allow a retry on the next state change.
                        changes.branches_for = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Keep the one-shot scoped capture fresh. The fetch key folds in the
    /// watch checksum, so any working-tree change (or commit — HEAD rides the
    /// checksum) re-captures; a context change (chat/scope/base) clears the
    /// stale content first so the pane shows the spinner, while a
    /// checksum-only refresh keeps the old diff visible until the new one
    /// lands.
    fn ensure_scoped(&mut self, cx: &mut Context<Self>) {
        if matches!(self.scope, DiffScope::WorkingTree | DiffScope::History) {
            self.scoped_inflight = None;
            self.scoped_task = None;
            return;
        }
        let Some(chat_id) = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.id.clone())
        else {
            return;
        };
        let Some(cwd) = self.scoped_cwd(cx) else {
            return;
        };
        let base = match self.scope {
            DiffScope::Branch => match &self.base_ref {
                Some(base) => Some(base.clone()),
                None => return, // branch list still loading
            },
            _ => None,
        };
        let commit_sha = match self.scope {
            DiffScope::Commit => match &self.commit {
                Some(commit) => Some(commit.sha.clone()),
                None => return, // a commit pane without its pin never fetches
            },
            _ => None,
        };
        let context = format!(
            "{}|{}|{}|{}|{}",
            chat_id,
            cwd,
            self.scope.mode(),
            base.as_deref().unwrap_or(""),
            commit_sha.as_deref().unwrap_or("")
        );
        let watch_sum = self.resolved(cx).map(|d| d.checksum).unwrap_or_default();
        let key = format!("{context}|{watch_sum}");
        if self.scoped_for.as_deref() == Some(key.as_str())
            || self.scoped_inflight.as_deref() == Some(key.as_str())
        {
            return;
        }
        if self
            .scoped_for
            .as_deref()
            .is_none_or(|prev| !prev.starts_with(&format!("{context}|")))
        {
            self.scoped = None;
            self.scoped_error = None;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mode = self.scope.mode();
        self.scoped_inflight = Some(key.clone());
        self.scoped_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("mode".into(), serde_json::Value::String(mode.to_string()));
            params.insert("chatId".into(), serde_json::Value::String(chat_id));
            if let Some(base) = base {
                params.insert("baseRef".into(), serde_json::Value::String(base));
            }
            if let Some(sha) = commit_sha {
                params.insert("commitSha".into(), serde_json::Value::String(sha));
            }
            let result = engine
                .client()
                .call(
                    methods::GET_CHECKOUT_DIFF,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |changes, cx| {
                if changes.scoped_inflight.as_deref() != Some(key.as_str()) {
                    return; // superseded
                }
                changes.scoped_inflight = None;
                match result.and_then(|value| {
                    serde_json::from_value::<CheckoutDiff>(value)
                        .map_err(|e| holt_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(diff) => {
                        changes.scoped = Some(diff);
                        changes.scoped_error = None;
                    }
                    Err(err) => {
                        changes.scoped = None;
                        changes.scoped_error = Some(err.to_string().into());
                    }
                }
                changes.scoped_for = Some(key);
                changes.sync(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            if scope == DiffScope::History {
                self.history_pane(cx)
                    .update(cx, |history, cx| history.ensure_loaded(cx));
            }
            self.sync(cx);
        }
        cx.notify();
    }

    fn history_pane(&mut self, cx: &mut Context<Self>) -> Entity<GitHistory> {
        if let Some(history) = &self.history {
            return history.clone();
        }
        let history = cx.new(|cx| GitHistory::new(self.state.clone(), cx));
        self.history_events =
            Some(
                cx.subscribe(&history, |this: &mut Self, _, event, cx| match event {
                    GitHistoryEvent::OpenCommit(commit) => {
                        // Bubble to the host — the surface strip opens the tab.
                        cx.emit(ChangesEvent::OpenCommit(commit.clone()));
                    }
                    GitHistoryEvent::FetchSucceeded => {
                        // Remote refs affect branch choices and every scoped diff
                        // based on a ref. Force fresh reads after the engine has
                        // also kicked its checkout-status watcher.
                        this.branches_for = None;
                        this.scoped_for = None;
                        this.scoped_inflight = None;
                        this.scoped_task = None;
                        this.ensure_branches(cx);
                        if this.scope != DiffScope::History {
                            this.ensure_scoped(cx);
                        }
                        cx.notify();
                    }
                }),
            );
        self.history = Some(history.clone());
        history
    }

    fn history_count(&mut self, cx: &mut Context<Self>) -> Entity<GitHistoryCount> {
        if let Some(count) = &self.history_count {
            return count.clone();
        }
        let history = self.history_pane(cx);
        let count = cx.new(|cx| GitHistoryCount::new(history, cx));
        self.history_count = Some(count.clone());
        count
    }

    fn history_fetch_button(&mut self, cx: &mut Context<Self>) -> Entity<GitHistoryFetchButton> {
        if let Some(button) = &self.history_fetch_button {
            return button.clone();
        }
        let history = self.history_pane(cx);
        let button = cx.new(|cx| GitHistoryFetchButton::new(history, cx));
        self.history_fetch_button = Some(button.clone());
        button
    }

    fn set_base_ref(&mut self, base: String, cx: &mut Context<Self>) {
        if self.base_ref.as_deref() != Some(base.as_str()) {
            self.base_ref = Some(base);
            self.sync(cx);
        }
        cx.notify();
    }

    /// Everything the pane needs kicked when (re)shown: the watch plus the
    /// scope-specific loads (branches, scoped/commit capture, history) — the
    /// shell's hook for freshly-mounted surface tabs.
    pub fn ensure_content(&mut self, cx: &mut Context<Self>) {
        self.sync(cx);
    }

    /// Reconcile parsed content with the currently-active diff.
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.discard_stale_draft(cx);
        // The watch is idempotent once started; a boot-deferred attempt
        // retries here too.
        self.ensure_watch(cx);
        if self.scope == DiffScope::History {
            self.history_pane(cx)
                .update(cx, |history, cx| history.ensure_loaded(cx));
            return;
        }
        if self.scope != DiffScope::Commit {
            self.ensure_branches(cx);
        }
        self.ensure_scoped(cx);
        let Some(diff) = self.active_diff(cx) else {
            if self.parsed.take().is_some() {
                self.rows.clear();
                self.row_ranges.clear();
                self.list.reset(0);
                self.folds.clear();
                self.highlights.clear();
                cx.notify();
            }
            return;
        };
        let key = self.parse_key(&diff);
        if self.parsed.as_ref().is_some_and(|p| p.key == key) {
            self.sync_comment_rows(cx);
            return;
        }
        // Parse off the render path — patches run to megabytes.
        let patch = diff.patch.clone();
        let truncated = diff.truncated;
        let additions = diff.additions;
        let deletions = diff.deletions;
        let file_count = diff.files.len();
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { parse_patch(&patch) })
                .await;
            this.update(cx, |changes, cx| {
                // Late results for a superseded diff are re-checked by key.
                let current = changes.active_diff(cx).map(|d| changes.parse_key(&d));
                if current.as_deref() != Some(key.as_str()) {
                    return;
                }
                let file_count = if file_count > 0 {
                    file_count
                } else {
                    files.len()
                };
                changes.folds.clear();
                changes.highlights.clear();
                let staged = changes.staged_comments(cx);
                let draft = changes.draft_anchor();
                let (rows, ranges) = flatten_rows(
                    &files,
                    &staged,
                    draft
                        .as_ref()
                        .map(|(path, side, line)| (path.as_str(), *side, *line)),
                    changes.mode,
                    |_| false,
                );
                changes.comment_key = comment_state_key(&staged, draft.as_ref());
                // The uniform hint keeps offsets for never-rendered rows
                // sane (most rows ARE lines); real heights land as rows
                // render.
                changes
                    .list
                    .reset_with_uniform_height(rows.len(), px(DIFF_LINE_HEIGHT));
                changes.rows = rows;
                changes.row_ranges = ranges;
                changes.parsed = Some(ParsedDiff {
                    key,
                    truncated,
                    additions,
                    deletions,
                    file_count,
                    files: Arc::new(files),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Swap one file's body rows (everything after its header) for
    /// `new_body`, splicing both the row model and the list state. gpui's
    /// `splice` shifts the logical scroll anchor by the count delta, so
    /// content below the fold stays put.
    fn replace_file_body(&mut self, file_ix: usize, new_body: Vec<DiffRow>) {
        let Some(range) = self.row_ranges.get(file_ix).cloned() else {
            return;
        };
        let body = range.start + 1..range.end;
        let delta = new_body.len() as isize - body.len() as isize;
        // Only splice the rows that moved: `ListState::splice` clamps the
        // scroll anchor to the range start when the anchored row is inside it,
        // so replacing a whole body jumped the pane to the top of the file.
        let (prefix, suffix) = {
            let old = &self.rows[body.clone()];
            let prefix = old
                .iter()
                .zip(&new_body)
                .take_while(|(a, b)| a == b)
                .count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new_body[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            (prefix, suffix)
        };
        if delta == 0 && prefix + suffix >= body.len() {
            return;
        }
        let changed = body.start + prefix..body.end - suffix;
        let mid: Vec<DiffRow> = new_body[prefix..new_body.len() - suffix].to_vec();
        self.list.splice(changed.clone(), mid.len());
        self.rows.splice(changed, mid);
        self.row_ranges[file_ix] = range.start..(range.end as isize + delta) as usize;
        for r in &mut self.row_ranges[file_ix + 1..] {
            *r = (r.start as isize + delta) as usize..(r.end as isize + delta) as usize;
        }
    }

    fn toggle_fold(&mut self, file_ix: usize, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let Some(file) = parsed.files.get(file_ix) else {
            return;
        };
        let expanded_height = body_height_with(
            file,
            &self.comments_for(&file.path, cx),
            self.draft_anchor_in(&file.path),
            self.mode,
        );
        let fold = self.folds.entry(file.path.clone()).or_default();
        let currently_collapsed = fold.collapsed;
        fold.from = if currently_collapsed {
            0.0
        } else {
            expanded_height
        };
        fold.to = if currently_collapsed {
            expanded_height
        } else {
            0.0
        };
        fold.collapsed = !currently_collapsed;
        fold.epoch += 1;
        fold.toggled_at = Some(std::time::Instant::now());
        // The body tweens as ONE clipped stand-in row; the settle sweep
        // swaps it for steady rows (all lines, or none) once the window
        // elapses.
        self.replace_file_body(
            file_ix,
            vec![DiffRow::FoldingBody {
                file: file_ix as u32,
            }],
        );
        self.ensure_fold_settle(cx);
    }

    /// Keep a sweep alive while any [`DiffRow::FoldingBody`] stand-ins
    /// remain; each tick settles the ones whose tween window has elapsed.
    fn ensure_fold_settle(&mut self, cx: &mut Context<Self>) {
        if self.fold_settle.is_some() {
            return;
        }
        self.fold_settle = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FOLD_TWEEN_WINDOW).await;
                let more = this
                    .update(cx, |changes, cx| changes.settle_folds(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |changes, _| changes.fold_settle = None)
                .ok();
        }));
    }

    /// Replace every settled folding stand-in with its steady-state rows.
    /// Returns whether any stand-ins are still mid-tween.
    fn settle_folds(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let files = parsed.files.clone();
        let mut pending = false;
        for file_ix in (0..self.row_ranges.len()).rev() {
            let range = &self.row_ranges[file_ix];
            let folding = self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                });
            if !folding {
                continue;
            }
            let Some(file) = files.get(file_ix) else {
                continue;
            };
            let fold = self.folds.get(&file.path).copied().unwrap_or_default();
            if fold.animating() {
                pending = true;
                continue;
            }
            let body = if fold.collapsed {
                Vec::new()
            } else {
                body_rows(
                    file_ix as u32,
                    file,
                    &self.comments_for(&file.path, cx),
                    self.draft_anchor_in(&file.path),
                    self.mode,
                )
            };
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
        pending
    }

    /// Every parsed file currently folded shut?
    fn all_collapsed(&self) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        !parsed.files.is_empty()
            && parsed.files.iter().all(|file| {
                self.folds
                    .get(&file.path)
                    .is_some_and(|fold| fold.collapsed)
            })
    }

    /// Collapse every file section, or expand them all when everything is
    /// already shut (the toolbar's fold button, t3code parity). Steady-state
    /// writes — no per-row tween arming, the whole list just snaps. List
    /// splices run bottom-up over the OLD ranges (each is O(log n)), then
    /// the row model rebuilds wholesale; the scroll anchor rides the
    /// splices, landing on the nearest file header when its body vanishes.
    fn toggle_collapse_all(&mut self, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let collapse = !self.all_collapsed();
        let files = parsed.files.clone();
        for file in files.iter() {
            let fold = self.folds.entry(file.path.clone()).or_default();
            fold.collapsed = collapse;
            fold.toggled_at = None;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let range = &self.row_ranges[file_ix];
            let body = range.start + 1..range.end;
            let new_len = if collapse {
                0
            } else {
                let file = &files[file_ix];
                let comments: Vec<DiffComment> = staged
                    .iter()
                    .filter(|comment| comment.path == file.path)
                    .cloned()
                    .collect();
                body_rows(
                    file_ix as u32,
                    file,
                    &comments,
                    self.draft_anchor_in(&file.path),
                    self.mode,
                )
                .len()
            };
            if body.len() != new_len {
                self.list.splice(body, new_len);
            }
        }
        let (rows, ranges) = flatten_rows(
            &files,
            &staged,
            draft
                .as_ref()
                .map(|(path, side, line)| (path.as_str(), *side, *line)),
            self.mode,
            |_| collapse,
        );
        self.rows = rows;
        self.row_ranges = ranges;
        cx.notify();
    }

    /// Swap unified ⇄ split (toolbar toggle). The parse is untouched — only
    /// the flattening changes — so this rebuilds the row model and re-anchors
    /// the scroll onto whichever file was under the viewport's top edge (row
    /// indices do not survive the re-pairing).
    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = self.mode.toggled();
        if let Some(dir) = self.state.read(cx).data_dir.clone() {
            let split = self.mode.is_split();
            cx.background_executor()
                .spawn(async move { persist_split(split, &dir) })
                .detach();
        }
        // A draft's `+` sits in a column that may not exist after the swap.
        self.hover = None;
        self.reflatten(cx);
    }

    fn reflatten(&mut self, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            cx.notify();
            return;
        };
        let files = parsed.files.clone();
        let top = self.list.logical_scroll_top().item_ix;
        let anchor_file = self
            .row_ranges
            .iter()
            .position(|range| range.contains(&top));
        let collapsed: Vec<bool> = files
            .iter()
            .map(|file| {
                self.folds
                    .get(&file.path)
                    .is_some_and(|fold| fold.collapsed)
            })
            .collect();
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        let (rows, ranges) = flatten_rows(
            &files,
            &staged,
            draft
                .as_ref()
                .map(|(path, side, line)| (path.as_str(), *side, *line)),
            self.mode,
            |ix| collapsed.get(ix).copied().unwrap_or(false),
        );
        self.list
            .reset_with_uniform_height(rows.len(), px(DIFF_LINE_HEIGHT));
        self.rows = rows;
        self.row_ranges = ranges;
        if let Some(start) = anchor_file
            .and_then(|ix| self.row_ranges.get(ix))
            .map(|r| r.start)
        {
            self.list.scroll_to_reveal_item(start);
        }
        cx.notify();
    }

    /// Cloned because rendering borrows `self` mutably a moment later.
    fn staged_comments(&self, cx: &App) -> Vec<DiffComment> {
        let state = self.state.read(cx);
        state.diff_comments(&state.composer_key()).to_vec()
    }

    fn comments_for(&self, path: &str, cx: &App) -> Vec<DiffComment> {
        self.staged_comments(cx)
            .into_iter()
            .filter(|comment| comment.path == path)
            .collect()
    }

    /// The parsed diff's pre-rename path for `path`, when the file moved.
    fn old_path_of(&self, path: &str) -> Option<String> {
        self.parsed
            .as_ref()?
            .files
            .iter()
            .find(|file| file.path == path)?
            .old_path
            .clone()
    }

    /// A draft belongs to the checkout it was opened over. Chat navigation
    /// swaps both the diff under it and the composer it would stage onto, so
    /// the half-written note is dropped rather than following the user across.
    fn discard_stale_draft(&mut self, cx: &mut Context<Self>) {
        let key = self.state.read(cx).composer_key();
        if self.draft.as_ref().is_some_and(|draft| draft.key != key) {
            self.draft = None;
            self.sync_comment_rows(cx);
            cx.notify();
        }
    }

    fn draft_anchor(&self) -> Option<(String, CommentSide, u32)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.path.clone(), draft.side, draft.line))
    }

    fn draft_anchor_in(&self, path: &str) -> Option<(CommentSide, u32)> {
        self.draft
            .as_ref()
            .filter(|draft| draft.path == path)
            .map(|draft| (draft.side, draft.line))
    }

    fn sync_comment_rows(&mut self, cx: &mut Context<Self>) {
        if self.parsed.is_none() {
            return;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        let key = comment_state_key(&staged, draft.as_ref());
        if key == self.comment_key {
            return;
        }
        self.comment_key = key;
        let Some(parsed) = &self.parsed else {
            return;
        };
        let files = parsed.files.clone();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let file = &files[file_ix];
            // A mid-tween stand-in is the settle sweep's to replace.
            if self
                .folds
                .get(&file.path)
                .is_some_and(|fold| fold.collapsed)
            {
                continue;
            }
            let range = &self.row_ranges[file_ix];
            if self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                })
            {
                continue;
            }
            let comments: Vec<DiffComment> = staged
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let body = body_rows(
                file_ix as u32,
                file,
                &comments,
                self.draft_anchor_in(&file.path),
                self.mode,
            );
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
    }

    fn set_hover(
        &mut self,
        path: &str,
        anchor: Option<(CommentSide, u32)>,
        cx: &mut Context<Self>,
    ) {
        let next = anchor.map(|(side, line)| HoverRow {
            path: path.to_string(),
            side,
            line,
        });
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    fn hovering(&self, path: &str, anchor: (CommentSide, u32)) -> bool {
        self.hover
            .as_ref()
            .is_some_and(|hover| hover.path == path && (hover.side, hover.line) == anchor)
    }

    fn clear_hover_at(&mut self, path: &str, anchor: (CommentSide, u32), cx: &mut Context<Self>) {
        if self.hovering(path, anchor) {
            self.hover = None;
            cx.notify();
        }
    }

    fn open_draft(
        &mut self,
        path: String,
        side: CommentSide,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| ComposerInput::new("Request a change…", cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_draft(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        let key = self.state.read(cx).composer_key();
        let old_path = self.old_path_of(&path);
        self.draft = Some(CommentDraft {
            key,
            path,
            old_path,
            side,
            line,
            input,
            _events: events,
        });
        window.focus(&handle, cx);
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = None;
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn commit_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        let body = draft.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            self.sync_comment_rows(cx);
            cx.notify();
            return;
        }
        let comment =
            DiffComment::new(draft.path, draft.side, draft.line, body).renamed_from(draft.old_path);
        // `draft.key`, not the live one: the note stages onto the composer it
        // was written against even if the selection moved under it.
        let key = draft.key;
        self.state.update(cx, |state, cx| {
            state.add_diff_comment(&key, comment);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn remove_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let key = state.composer_key();
            state.remove_diff_comment(&key, id);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn close_scope_menu(&mut self, cx: &mut Context<Self>) {
        if self.scope_menu.begin_close() {
            popover::reap_popup(cx, |changes: &mut Self| &mut changes.scope_menu);
        }
    }

    fn close_ref_menu(&mut self, cx: &mut Context<Self>) {
        if self.ref_menu.begin_close() {
            popover::reap_popup(cx, |changes: &mut Self| &mut changes.ref_menu);
        }
    }

    fn open_ref_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // "PaletteSearch" context: ↑↓/⏎ stay unbound in the input and bubble
        // to the card's key handler.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search branches…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(menu) = this.ref_menu.open_mut() {
                    menu.active = 0;
                }
                cx.notify();
            }
        });
        let handle = search.read(cx).focus_handle(cx);
        // The highlight starts ON the current base (query is empty, so the
        // filtered rows are just the branch list).
        let active = self
            .base_ref
            .as_ref()
            .and_then(|base| self.branches.iter().position(|b| b == base))
            .unwrap_or(0);
        self.ref_menu.open(RefMenu {
            search,
            active,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            _search_events: search_events,
        });
        // Focusable before first paint (the add-space palette's proven order).
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Filtered branch indices for the open ref menu (ranked substring match).
    fn ref_menu_rows(&self, cx: &App) -> Vec<usize> {
        let query = self
            .ref_menu
            .get()
            .map(|menu| menu.search.read(cx).text().to_string())
            .unwrap_or_default();
        popover::filter_indices(&query, &self.branches)
    }

    /// Dropdown keys (bubbling from the focused search input): ↑↓ navigate,
    /// ⏎ picks the highlighted branch, Esc closes.
    fn ref_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // The card stays mounted (and focused) through the exit animation —
        // keys must not drive a dying menu.
        if !self.ref_menu.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => self.close_ref_menu(cx),
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.ref_menu_rows(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.ref_menu.open_mut() {
                    menu.active = popover::menu_step(Some(menu.active), count, delta).unwrap_or(0);
                    menu.list_scroll.scroll_to_item(menu.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let active = self.ref_menu.get().map(|m| m.active).unwrap_or(0);
                let pick = self
                    .ref_menu_rows(cx)
                    .get(active)
                    .and_then(|ix| self.branches.get(*ix).cloned());
                if let Some(branch) = pick {
                    self.set_base_ref(branch, cx);
                    self.close_ref_menu(cx);
                }
            }
            _ => {}
        }
    }

    /// Start excerpt parsing and a lazy full-source fetch for an expanded file.
    fn request_highlight(
        &mut self,
        file: &FileDiff,
        parsed_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<DiffHighlights>> {
        let lang = holt_syntax::language_for_path(&file.path)?;
        let fingerprint = hash64(&[parsed_key, &file.path]);
        if let Some(slot) = self.highlights.get(&file.path)
            && slot.fingerprint == fingerprint
        {
            return match &slot.state {
                DiffHighlightState::Ready(highlights) | DiffHighlightState::Excerpt(highlights) => {
                    Some(highlights.clone())
                }
                DiffHighlightState::Pending | DiffHighlightState::Plain => None,
            };
        }
        if !holt_syntax::supports_language(lang) {
            self.highlights.insert(
                file.path.clone(),
                HighlightSlot {
                    fingerprint,
                    state: DiffHighlightState::Plain,
                    _excerpt_task: None,
                    _fetch_task: None,
                },
            );
            return None;
        }
        let path = file.path.clone();
        let excerpt_file = file.clone();
        let excerpt_path = path.clone();
        let excerpt_task = cx.spawn(async move |this, cx| {
            let highlights = cx
                .background_executor()
                .spawn(async move { excerpt_highlights(&excerpt_file, lang).map(Arc::new) })
                .await;
            this.update(cx, |changes, cx| {
                if let Some(slot) = changes.highlights.get_mut(&excerpt_path)
                    && slot.fingerprint == fingerprint
                    && matches!(slot.state, DiffHighlightState::Pending)
                {
                    slot.state = match highlights {
                        Some(highlights) => DiffHighlightState::Excerpt(highlights),
                        None => DiffHighlightState::Plain,
                    };
                    cx.notify();
                }
            })
            .ok();
        });

        let active = self.active_diff(cx);
        let engine = self.state.read(cx).engine().cloned();
        let chat_id = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.id.clone());
        let mode = self.scope.mode().to_string();
        let base_ref = self.base_ref.clone();
        let commit_sha = (self.scope == DiffScope::Commit)
            .then(|| self.commit.as_ref().map(|commit| commit.sha.clone()))
            .flatten();
        let fetch_file = file.clone();
        let fetch_path = path.clone();
        let fetch_task = match (active, engine) {
            (Some(diff), Some(engine)) => Some(cx.spawn(async move |this, cx| {
                let request = holt_proto::GetCheckoutFileDiffTextRequest {
                    checkout_id: diff.checkout_id,
                    cwd: diff.cwd,
                    path: fetch_path.clone(),
                    mode,
                    base_ref,
                    chat_id,
                    commit_sha,
                    diff_checksum: diff.checksum,
                };
                let params = serde_json::to_value(request)
                    .ok()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                let response = engine
                    .client()
                    .call(
                        methods::GET_CHECKOUT_FILE_DIFF_TEXT,
                        serde_json::Value::Object(params),
                    )
                    .await
                    .ok()
                    .and_then(|value| {
                        serde_json::from_value::<holt_proto::CheckoutFileDiffText>(value).ok()
                    });
                let highlights = match response {
                    Some(response) => {
                        cx.background_executor()
                            .spawn(async move {
                                full_highlights(&fetch_file, lang, &response).map(Arc::new)
                            })
                            .await
                    }
                    None => None,
                };
                this.update(cx, |changes, cx| {
                    if let Some(slot) = changes.highlights.get_mut(&fetch_path)
                        && slot.fingerprint == fingerprint
                        && let Some(highlights) = highlights
                    {
                        slot.state = DiffHighlightState::Ready(highlights);
                        cx.notify();
                    }
                })
                .ok();
            })),
            _ => None,
        };
        self.highlights.insert(
            file.path.clone(),
            HighlightSlot {
                fingerprint,
                state: DiffHighlightState::Pending,
                _excerpt_task: Some(excerpt_task),
                _fetch_task: fetch_task,
            },
        );
        None
    }

    // ---- rendering ----

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(parsed) = &self.parsed else {
            return gpui::Empty.into_any_element();
        };
        let files = parsed.files.clone();
        let parsed_key = parsed.key.clone();
        let Some(row) = self.rows.get(ix).copied() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        match row {
            DiffRow::FileHeader { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                self.render_file_header(
                    file as usize,
                    file_diff,
                    &fold,
                    FileHeaderPresentation::Row,
                    &theme,
                    cx,
                )
            }
            DiffRow::Notice { file, notice } => files
                .get(file as usize)
                .and_then(|f| file_notices(f).into_iter().nth(notice as usize))
                .map(|text| notice_row(text, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::HunkHeader { file, hunk } => files
                .get(file as usize)
                .and_then(|f| f.hunks.get(hunk as usize))
                .map(|h| hunk_header_row(&h.header, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::Line {
                file,
                hunk,
                line,
                flat: _,
            } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let Some(line) = file_diff
                    .hunks
                    .get(hunk as usize)
                    .and_then(|h| h.lines.get(line as usize))
                else {
                    return gpui::Empty.into_any_element();
                };
                let spans = highlight
                    .as_deref()
                    .map(|highlights| highlights.spans(line))
                    .unwrap_or(&[]);
                let gutter_px = gutter_width(file_diff);
                let row = diff_line_row(line, spans, &theme, gutter_px);
                let Some((side, line_no)) = line_anchor(line) else {
                    return row;
                };
                let path = file_diff.path.clone();
                let hovered = self.hovering(&path, (side, line_no));
                let move_path = path.clone();
                let leave_path = path.clone();
                div()
                    .id(("diff-line", ix))
                    .w_full()
                    .relative()
                    .child(row)
                    .when(hovered, |el| {
                        el.child(positioned_adder(
                            comment_adder_left(side, gutter_px),
                            render_comment_adder(&path, side, line_no, &theme, cx),
                        ))
                    })
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        this.set_hover(&move_path, Some((side, line_no)), cx);
                    }))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.clear_hover_at(&leave_path, (side, line_no), cx);
                        }
                    }))
                    .into_any_element()
            }
            DiffRow::SplitLine {
                file,
                hunk,
                left,
                right,
            } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let Some(lines) = file_diff.hunks.get(hunk as usize).map(|h| &h.lines) else {
                    return gpui::Empty.into_any_element();
                };
                let gutter_px = gutter_width(file_diff);
                // Same slot on both sides = a context row: one line, drawn in
                // both columns.
                let mirrored = left.is_some() && left == right;
                let left = left.and_then(|slot| lines.get(slot as usize));
                let right = right.and_then(|slot| lines.get(slot as usize));
                // `\ No newline at end of file` is not code on one side — it
                // is a note about the row, so it spans both columns. Pairing
                // never puts a marker opposite code, so either side having one
                // means the whole row is the marker.
                if let Some(line) = [left, right]
                    .into_iter()
                    .flatten()
                    .find(|line| line.kind == LineKind::Meta)
                {
                    return meta_line_row(&line.text, &theme, 2.0 * (ACCENT_BAR_WIDTH + gutter_px));
                }
                // Refcounted, not cloned per listener: a split row wires up to
                // four of them, and this runs for every row in the viewport
                // plus the list's overdraw, every frame.
                let path: SharedString = file_diff.path.clone().into();
                // A mirrored row's columns carry the same text and the same
                // spans, so the runs are built once and shared — context is
                // most of a diff, so this is most of the rows.
                let shared_runs = mirrored
                    .then(|| left.map(|line| line_runs(line, highlight.as_deref(), &theme)))
                    .flatten();
                let cell = |line: Option<&DiffLine>, old: bool| {
                    line.map(|line| {
                        let runs = shared_runs
                            .clone()
                            .unwrap_or_else(|| line_runs(line, highlight.as_deref(), &theme));
                        let number = if old { line.old_no } else { line.new_no };
                        split_line_cell(line, number, runs, &theme, gutter_px)
                    })
                };
                // The left column is inert. It shows the pre-change file, and
                // a deleted line is not there to be changed — a note on it
                // would cite a line the agent cannot edit. Everything is cited
                // against the new file, so only the right column takes a `+`.
                // Cards for old-side notes still render (they are pushed by
                // the row, not the column), so switching layouts never hides
                // one that is already staged.
                let left = cell(left, true)
                    .map(IntoElement::into_any_element)
                    .unwrap_or_else(|| split_filler().into_any_element());
                let right = match (cell(right, false), right.and_then(line_anchor)) {
                    (Some(cell), Some(anchor)) => {
                        let (side, line_no) = anchor;
                        let (move_path, leave_path) = (path.clone(), path.clone());
                        cell.id(("split-new", ix))
                            .when(self.hovering(&path, anchor), |el| {
                                el.relative().child(positioned_adder(
                                    split_adder_left(gutter_px),
                                    render_comment_adder(&path, side, line_no, &theme, cx),
                                ))
                            })
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                this.set_hover(&move_path, Some(anchor), cx);
                            }))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if !*hovered {
                                    this.clear_hover_at(&leave_path, anchor, cx);
                                }
                            }))
                            .into_any_element()
                    }
                    (Some(cell), None) => cell.into_any_element(),
                    (None, _) => split_filler().into_any_element(),
                };
                split_row(left, right).into_any_element()
            }
            DiffRow::CommentCard { file, card } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let comments = self.comments_for(&file_diff.path, cx);
                match comments.get(card as usize) {
                    Some(comment) => render_comment_card(comment, &theme, cx),
                    None => gpui::Empty.into_any_element(),
                }
            }
            DiffRow::CommentDraft { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                match self
                    .draft
                    .as_ref()
                    .filter(|draft| draft.path == file_diff.path)
                {
                    // Header cites the same path the staged card and the
                    // prompt bullet will.
                    Some(draft) => render_comment_draft(
                        draft_cite_path(draft),
                        draft.line,
                        draft.input.clone(),
                        &theme,
                        cx,
                    ),
                    None => gpui::Empty.into_any_element(),
                }
            }
            DiffRow::BodyPad { .. } => div().w_full().h(px(BODY_BOTTOM_PAD)).into_any_element(),
            DiffRow::FoldingBody { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let (from, to) = (fold.from, fold.to);
                // Only the revealable slice is built — the tween never pays
                // for lines it cannot show.
                let cap = from.max(to).min(FOLD_TWEEN_MAX_PX);
                let body = render_file_body_upto(file_diff, highlight, &theme, cap, self.mode);
                let clipped = div().w_full().overflow_hidden().child(body);
                if fold.animating() {
                    clipped
                        .with_animation(
                            SharedString::from(format!("fold-{}-{}", file_diff.path, fold.epoch)),
                            COLLAPSE.animation(),
                            move |el, t| el.h(px(motion::lerp(from, to, t))),
                        )
                        .into_any_element()
                } else {
                    // Post-tween, pre-settle: hold the full target height so
                    // the settle splice swaps rows without any reflow (the
                    // capped slice always covers what the viewport can see —
                    // tweens start from a clicked, on-screen header).
                    clipped.h(px(to)).into_any_element()
                }
            }
        }
    }

    fn render_file_header(
        &mut self,
        ix: usize,
        file: &FileDiff,
        fold: &FileFold,
        presentation: FileHeaderPresentation,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = fold.collapsed;
        let path = file.path.clone();
        let adds = file.additions;
        let dels = file.deletions;
        let sticky = presentation == FileHeaderPresentation::Sticky;
        let sticky_paint = sticky.then(|| sticky_file_header_paint(theme));
        let rest_bg = if let Some(paint) = sticky_paint {
            paint.rest_bg
        } else {
            theme.ink(0.025)
        };
        let hover_bg = if let Some(paint) = sticky_paint {
            paint.hover_bg
        } else {
            theme.ink(0.05)
        };

        // Chevron (holt checkout-diff-sidebar): chevron-right closed,
        // chevron-down open; gpui divs have no rotation transform at the
        // pinned rev, so the glyph swap crossfades over the same 200 ms.
        let chevron_icon = if collapsed {
            crate::icons::ALT_ARROW_RIGHT
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        let chevron = div().flex_none().size(px(14.0)).child(
            crate::icons::icon(chevron_icon)
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
        );
        let chevron: AnyElement = if fold.animating() {
            chevron
                .with_animation(
                    SharedString::from(format!(
                        "chev-{}-{path}-{}",
                        presentation.key_prefix(),
                        fold.epoch
                    )),
                    CHEVRON.animation(),
                    |el, t| el.opacity(0.25 + 0.75 * t),
                )
                .into_any_element()
        } else {
            chevron.into_any_element()
        };

        // Header row: chevron + mono path (one quiet tone) + right-aligned
        // +N / −N counts on a slightly raised wash. The header carries the
        // section separator (the per-file wrapper it used to hang on is
        // gone — rows are flat now).
        div()
            .id(presentation.element_id(ix))
            .w_full()
            .h(px(FILE_HEADER_HEIGHT))
            .when(
                presentation == FileHeaderPresentation::Row && ix > 0,
                |el| el.border_t_1().border_color(crate::theme::hairline(0.04)),
            )
            .when(sticky, |el| {
                el.border_b_1()
                    .border_color(sticky_paint.expect("sticky paint").border)
                    .block_mouse_except_scroll()
            })
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .bg(rest_bg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(ix, cx);
                cx.notify();
            }))
            .child(chevron)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(file.path.clone())),
            )
            .when(file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("BIN")),
                )
            })
            .when(adds > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{adds}"))),
                )
            })
            .when(dels > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{dels}"))),
                )
            })
            .into_any_element()
    }

    fn render_sticky_file_header(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scroll_top = self.list.logical_scroll_top();
        let sticky = sticky_file_header(
            &self.row_ranges,
            scroll_top.item_ix,
            scroll_top.offset_in_item.as_f32(),
        )?;
        debug_assert_eq!(
            self.rows.get(sticky.header_row),
            Some(&DiffRow::FileHeader {
                file: sticky.file_ix as u32,
            })
        );
        let files = self.parsed.as_ref()?.files.clone();
        let file = files.get(sticky.file_ix)?;
        let fold = self.folds.get(&file.path).copied().unwrap_or_default();
        let next_header_y = sticky.next_header_row.and_then(|row| {
            let bounds = self.list.bounds_for_item(row)?;
            let viewport = self.list.viewport_bounds();
            Some((bounds.origin.y - viewport.origin.y).as_f32())
        });
        let top_offset = sticky_header_push_offset(next_header_y);
        let header = self.render_file_header(
            sticky.file_ix,
            file,
            &fold,
            FileHeaderPresentation::Sticky,
            theme,
            cx,
        );
        let paint = sticky_file_header_paint(theme);
        // The sticky floats over diff rows, but it belongs to the same content
        // plane. Tint the blur with `theme.bg`; `glass_overlay` is deliberately
        // reserved for elevated menus/cards and produced the wrong hue here.
        let header = if let Some(tint) = paint.frost_tint {
            div().w_full().bg(tint).child(header).into_any_element()
        } else {
            header
        };
        // Frosted is a pass-through when the resolved surface is opaque.
        let header = crate::frost::frosted(0.0, STICKY_FILE_HEADER_BLUR, header);

        Some(
            div()
                .absolute()
                .top(px(top_offset))
                .left_0()
                .w_full()
                .child(header)
                .into_any_element(),
        )
    }

    /// A small hover-washed icon button for the pane header. The header lives
    /// inside the titlebar drag strip, so the button occludes and swallows the
    /// mouse-down (same discipline as the shell's `header_icon_button`).
    fn header_button(
        id: &'static str,
        icon_path: &'static str,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        Self::header_toggle(id, icon_path, false, theme)
    }

    /// [`Self::header_button`] with a latched look: an `active` toggle holds
    /// the hover wash and the full text tone, so the pane says which layout
    /// it is in without a label.
    fn header_toggle(
        id: &'static str,
        icon_path: &'static str,
        active: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            // Latched: the blend is neither read nor driven, and its listener
            // would dirty the whole window on every enter/leave for nothing.
            .map(|el| {
                if active {
                    el.bg(crate::theme::wash(0.14))
                } else {
                    el.bg(motion::hover_blend(
                        id,
                        crate::theme::wash(0.0),
                        crate::theme::wash(0.14),
                    ))
                    .on_hover(motion::hover_listener(id))
                }
            })
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .child(
                crate::icons::icon(icon_path)
                    .size(px(14.0))
                    .text_color(if active {
                        theme.text
                    } else {
                        theme.text_muted.opacity(0.7)
                    }),
            )
    }

    /// The unified ⇄ split layout toggle (both the scoped and the
    /// commit-pinned toolbars carry it).
    fn split_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        Self::header_toggle(
            "changes-split",
            crate::icons::SPLIT_COLUMNS,
            self.mode.is_split(),
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_mode(cx);
        }))
        .into_any_element()
    }

    /// The pane-header controls: scope dropdown, `{branch} → {base ⌄}` ref
    /// selector (branch scope), fold-all. Rendered BY THE SHELL inside the
    /// session titlebar's trailing section (the band above the pane) — the
    /// titlebar overlay owns that strip's hit-testing, so controls mounted
    /// under it would never see a click. The expand and close buttons ride
    /// alongside, shell-owned (they mutate shell state).
    pub fn render_header_controls(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // Commit-pinned pane: the pin never changes, so a fixed identity
        // chip (mono short sha + subject) replaces the scope dropdown;
        // fold-all still trails.
        if let Some(commit) = self.commit.clone() {
            let short: String = commit.sha.chars().take(7).collect();
            return div()
                .size_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .h(px(22.0))
                        .px(px(6.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .bg(crate::theme::ink(0.05))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(short)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .child(SharedString::from(commit.subject.clone())),
                )
                .child(self.split_toggle(&theme, cx))
                .child(
                    Self::header_button("changes-fold-all", crate::icons::FOLD_VERTICAL, &theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_collapse_all(cx);
                        })),
                )
                .into_any_element();
        }
        let scope = self.scope;
        let history_count = (scope == DiffScope::History).then(|| self.history_count(cx));
        let history_fetch_button =
            (scope == DiffScope::History).then(|| self.history_fetch_button(cx));
        let trigger = div()
            .id("changes-scope-trigger")
            .h(px(24.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "changes-scope-trigger",
                crate::theme::wash(0.05),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener("changes-scope-trigger"))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.scope_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.scope_menu.take_press_was_open() {
                    this.close_scope_menu(cx);
                } else {
                    this.scope_menu.open(());
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(SharedString::from(scope.label())),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        let trigger = if self.scope_menu.get().is_some() {
            let closing = self.scope_menu.closing_since();
            let menu = self.render_scope_menu(&theme, cx);
            trigger.relative().child(popover::anchored_menu_below_gap(
                "changes-scope-menu",
                menu,
                closing,
                10.0,
            ))
        } else {
            trigger
        };

        let trailing: AnyElement = if scope == DiffScope::History {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(2.0))
                .children(history_fetch_button)
                .child(
                    Self::header_button("history-refresh", crate::icons::REFRESH, &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.history_pane(cx)
                                .update(cx, |history, cx| history.refresh(cx));
                        }),
                    ),
                )
                .into_any_element()
        } else {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(self.split_toggle(&theme, cx))
                .child(
                    Self::header_button("changes-fold-all", crate::icons::FOLD_VERTICAL, &theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_collapse_all(cx);
                        })),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(trigger)
            .when_some(history_count, |element, count| {
                element.child(div().flex_1().min_w_0().h_full().child(count))
            })
            .children(self.render_ref_selector(&theme, cx))
            .when(scope != DiffScope::History, |element| {
                element.child(div().flex_1())
            })
            .child(trailing)
            .into_any_element()
    }

    fn render_scope_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.scope;
        popover::popover_card(theme)
            .w(px(180.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_scope_menu(cx)))
            .child(
                // The 2px row gap every other menu carries — rows straight on
                // the card abutted, adjacent washes read as one slab (user
                // report).
                div().flex().flex_col().gap(px(2.0)).children(
                    DiffScope::ALL.into_iter().enumerate().map(|(ix, scope)| {
                        popover::menu_row(
                            theme,
                            scope == current,
                            format!("changes-scope-row-{ix}"),
                        )
                        .id(("changes-scope-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_scope(scope, cx);
                            this.close_scope_menu(cx);
                        }))
                        .child(div().flex_1().child(SharedString::from(scope.label())))
                    }),
                ),
            )
            .into_any_element()
    }

    /// `{branch} → {base ⌄}` — which ref the branch scope compares against
    /// (t3code's ref strip), inlined into the pane header. Branch scope only.
    fn render_ref_selector(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.scope != DiffScope::Branch {
            return None;
        }
        let branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.branch.clone())
            .unwrap_or_else(|| "HEAD".to_string());
        let base = self.base_ref.clone().unwrap_or_else(|| "…".to_string());
        // Even truncation: taffy shrinks flex items ∝ factor × basis, and the
        // default factor of 1 splits the deficit proportionally to content —
        // a long branch stayed near-whole while a short base ("main") read as
        // a bare ellipsis (user report). Weighting each side's factor by its
        // own length SQUARED (mono font, so chars ∝ px) lands the deficit
        // ~cubically on the longer name: the short side's loss stays
        // sub-pixel even under a big deficit (a linear weight still cost it
        // a char), while equal lengths still split evenly.
        let branch_weight = (branch.chars().count().max(1) as f32).powi(2);
        let base_weight = (base.chars().count().max(1) as f32).powi(2);
        let trigger = div()
            .id("changes-ref-trigger")
            .h(px(22.0))
            .px(px(6.0))
            // Shrinkable, like the branch label beside it — a flex_none
            // trigger with a long base name plowed over the header buttons
            // (user report); both sides truncate instead.
            .min_w_0()
            .flex_shrink(base_weight)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "changes-ref-trigger",
                crate::theme::wash(0.0),
                crate::theme::wash(0.12),
            ))
            .on_hover(motion::hover_listener("changes-ref-trigger"))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.ref_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                if this.ref_menu.take_press_was_open() {
                    this.close_ref_menu(cx);
                    cx.notify();
                } else {
                    this.open_ref_menu(window, cx);
                }
            }))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .text_color(theme.text)
                    .child(SharedString::from(base)),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        let trigger = if self.ref_menu.get().is_some() {
            let closing = self.ref_menu.closing_since();
            let menu = self.render_ref_menu(theme, cx);
            trigger.relative().child(popover::anchored_menu_below_gap(
                "changes-ref-menu",
                menu,
                closing,
                10.0,
            ))
        } else {
            trigger
        };
        Some(
            div()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                // Extra room off the scope dropdown (row gap alone read
                // cramped — user report).
                .ml(px(6.0))
                .child(
                    div()
                        .min_w_0()
                        .flex_shrink(branch_weight)
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.5))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(branch)),
                )
                .child(
                    crate::icons::icon(crate::icons::ARROW_RIGHT)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_faint),
                )
                .child(trigger)
                .into_any_element(),
        )
    }

    fn render_ref_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (search, active, focus, list_scroll) = {
            let Some(menu) = self.ref_menu.get() else {
                return div().into_any_element();
            };
            (
                menu.search.clone(),
                menu.active,
                menu.focus.clone(),
                menu.list_scroll.clone(),
            )
        };
        let rows = self.ref_menu_rows(cx);
        let current = self.base_ref.clone();
        let branches = self.branches.clone();

        let list: AnyElement = if rows.is_empty() {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(if branches.is_empty() {
                    "No branches"
                } else {
                    "No matching branches"
                }))
                .into_any_element()
        } else {
            div()
                .id("changes-ref-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(240.0))
                .overflow_y_scroll()
                .track_scroll(&list_scroll)
                .children(rows.into_iter().enumerate().map(|(row_ix, branch_ix)| {
                    let name = branches[branch_ix].clone();
                    let selected = current.as_deref() == Some(name.as_str());
                    let label = name.clone();
                    popover::menu_row_nav(
                        theme,
                        selected,
                        row_ix == active,
                        format!("changes-ref-row-{row_ix}"),
                    )
                    .id(("changes-ref-row", row_ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_base_ref(name.clone(), cx);
                        this.close_ref_menu(cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(12.0))
                            .child(SharedString::from(label)),
                    )
                }))
                .into_any_element()
        };

        popover::popover_card(theme)
            .w(px(240.0))
            .track_focus(&focus)
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| this.ref_menu_key(event, cx)),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_ref_menu(cx)))
            .flex()
            .flex_col()
            .child(popover::search_input_frame(
                theme,
                search.into_any_element(),
            ))
            .child(list)
            .into_any_element()
    }

    fn render_header_strip(&self, theme: &Theme) -> Option<AnyElement> {
        let parsed = self.parsed.as_ref()?;
        Some(
            div()
                .flex_none()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(Theme::SPACE_LG))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(scope_label(
                            self.scope,
                            parsed.file_count,
                            self.base_ref.as_deref(),
                        ))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{}", parsed.additions))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{}", parsed.deletions))),
                )
                .child(div().flex_1())
                .when(parsed.truncated, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(theme.warning.opacity(0.08))
                            .text_color(theme.warning.opacity(0.75))
                            .child(SharedString::from("Partial snapshot")),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Green for additions — sampled from the reference diff (soft emerald).
fn add_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_add // emerald-400
}

/// Red for deletions — softer than the theme danger, per the reference diff.
fn del_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_del // red-400
}

/// One notice row ("New file", "Binary file — contents not shown", …).
fn notice_row(notice: String, theme: &Theme) -> AnyElement {
    div()
        .h(px(NOTICE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(notice))
        .into_any_element()
}

/// One `@@ … @@` hunk-header row on the bluish-grey wash.
fn hunk_header_row(header: &str, theme: &Theme) -> AnyElement {
    div()
        .h(px(HUNK_HEADER_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .bg(theme.diff_hunk_bg)
        .font_family(theme.font_mono.clone())
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(header.to_string()))
        .into_any_element()
}

/// One +/−/context/meta diff line: coloured accent bar, dual line-number
/// gutters (`gutter_px` wide — see [`gutter_width`]), marker column, and
/// paint-only syntax runs.
fn diff_line_row(
    line: &DiffLine,
    spans: &[holt_syntax::HighlightSpan],
    theme: &Theme,
    gutter_px: f32,
) -> AnyElement {
    if line.kind == LineKind::Meta {
        return meta_line_row(
            &line.text,
            theme,
            ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0,
        );
    }

    // Row tints sampled from the reference: ~5–6% washes over the pane tone.
    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;

    let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
        LineKind::Add => (
            "+",
            add_color(theme),
            Some(add_bg),
            Some(add_color(theme).opacity(0.55)),
            add_color(theme).opacity(0.9),
        ),
        LineKind::Del => (
            "−",
            del_color(theme),
            Some(del_bg),
            Some(del_color(theme).opacity(0.55)),
            del_color(theme).opacity(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.opacity(0.5),
            None,
            None,
            theme.text_faint.opacity(0.8),
        ),
    };
    let gutter = |no: Option<u32>, color: gpui::Hsla| {
        div()
            .w(px(gutter_px))
            .flex_none()
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .text_color(color)
            .flex()
            .justify_end()
            .pr(px(8.0))
            .child(SharedString::from(
                no.map(|n| n.to_string()).unwrap_or_default(),
            ))
    };
    let mono = font(theme.font_mono.clone());
    let runs = render::runs_for_syntax_line_with_plain(
        &line.text,
        spans,
        &mono,
        theme.text.opacity(0.92),
        theme,
    );
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when_some(row_bg, |el, bg| el.bg(bg))
        // Accent bar: solid colour on +/− rows, invisible spacer on
        // context rows so columns always align.
        .child(
            div()
                .w(px(ACCENT_BAR_WIDTH))
                .h_full()
                .flex_none()
                .when_some(accent, |el, color| el.bg(color)),
        )
        .child(gutter(
            line.old_no,
            if line.kind == LineKind::Del {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(gutter(
            line.new_no,
            if line.kind == LineKind::Add {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(DIFF_TEXT_SIZE))
                .text_color(marker_color)
                .font_family(theme.font_mono.clone())
                .child(SharedString::from(marker)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .pl(px(12.0))
                .font_family(theme.font_mono.clone())
                .text_size(px(DIFF_TEXT_SIZE))
                .whitespace_nowrap()
                .child(gpui::StyledText::new(line.text.clone()).with_runs(runs)),
        )
        .into_any_element()
}

/// `\ No newline at end of file` and friends: a note about the row rather
/// than code, so it is indented past the columns and never tinted. In split
/// mode it spans both halves.
fn meta_line_row(text: &str, theme: &Theme, pad_left: f32) -> AnyElement {
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .pl(px(pad_left))
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .italic()
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Split (side-by-side) rendering
// ---------------------------------------------------------------------------

/// The paint-only syntax runs for one diff line.
fn line_runs(
    line: &DiffLine,
    highlights: Option<&DiffHighlights>,
    theme: &Theme,
) -> Vec<gpui::TextRun> {
    let spans = highlights.map(|h| h.spans(line)).unwrap_or(&[]);
    render::runs_for_syntax_line_with_plain(
        &line.text,
        spans,
        &font(theme.font_mono.clone()),
        theme.text.opacity(0.92),
        theme,
    )
}

/// One half of a split row: the same accent bar / gutter / marker / code
/// columns a unified row uses, minus the second gutter — each half numbers
/// only its own side. Takes prebuilt `runs` so a mirrored row can share one
/// set across both columns.
fn split_line_cell(
    line: &DiffLine,
    number: Option<u32>,
    runs: Vec<gpui::TextRun>,
    theme: &Theme,
    gutter_px: f32,
) -> gpui::Div {
    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;
    let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
        LineKind::Add => (
            "+",
            add_color(theme),
            Some(add_bg),
            Some(add_color(theme).opacity(0.55)),
            add_color(theme).opacity(0.9),
        ),
        LineKind::Del => (
            "−",
            del_color(theme),
            Some(del_bg),
            Some(del_color(theme).opacity(0.55)),
            del_color(theme).opacity(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.opacity(0.5),
            None,
            None,
            theme.text_faint.opacity(0.8),
        ),
    };
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .flex()
        .flex_row()
        .items_center()
        .when_some(row_bg, |el, bg| el.bg(bg))
        .child(
            div()
                .w(px(ACCENT_BAR_WIDTH))
                .h_full()
                .flex_none()
                .when_some(accent, |el, color| el.bg(color)),
        )
        .child(
            div()
                .w(px(gutter_px))
                .flex_none()
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(number_color)
                .flex()
                .justify_end()
                .pr(px(8.0))
                .child(SharedString::from(
                    number.map(|n| n.to_string()).unwrap_or_default(),
                )),
        )
        .child(
            div()
                .w(px(SPLIT_MARKER_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(DIFF_TEXT_SIZE))
                .text_color(marker_color)
                .font_family(theme.font_mono.clone())
                .child(SharedString::from(marker)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .pl(px(6.0))
                .font_family(theme.font_mono.clone())
                .text_size(px(DIFF_TEXT_SIZE))
                .whitespace_nowrap()
                .child(gpui::StyledText::new(line.text.clone()).with_runs(runs)),
        )
}

/// The empty half of a one-sided split row — a pure-insert row has no old
/// line, and vice versa. A flat wash, quieter than either tint, reads as
/// "nothing here" without competing with the code beside it.
fn split_filler() -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(crate::theme::ink(0.03))
}

/// Compose the two halves with the centre hairline.
fn split_row(left: AnyElement, right: AnyElement) -> gpui::Div {
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_stretch()
        .child(left)
        .child(
            div()
                .w(px(SPLIT_DIVIDER_WIDTH))
                .h_full()
                .flex_none()
                .bg(crate::theme::hairline(0.06)),
        )
        .child(right)
}

pub const COMMENT_ADDER_SIZE: f32 = 16.0;

/// A split row's `+` only ever appears in the right column, which carries one
/// gutter — so the offset is the same for every line. It is measured from the
/// column, not the row: the halves are fluid, so the right one has no
/// absolute left edge to measure from.
pub fn split_adder_left(gutter_px: f32) -> f32 {
    ACCENT_BAR_WIDTH + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

/// A unified row carries both gutters side by side, and a deletion numbers in
/// the first.
pub fn comment_adder_left(side: CommentSide, gutter_px: f32) -> f32 {
    let column = match side {
        CommentSide::Old => 0.0,
        CommentSide::New => gutter_px,
    };
    ACCENT_BAR_WIDTH + column + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

fn positioned_adder(left: f32, adder: AnyElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left))
        .top(px(0.0))
        .h_full()
        .flex()
        .items_center()
        .child(adder)
}

fn render_comment_adder(
    path: &str,
    side: CommentSide,
    line: u32,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let target = path.to_string();
    div()
        .id(SharedString::from(format!(
            "cmt-add-{path}-{}-{line}",
            side.tag()
        )))
        .size(px(COMMENT_ADDER_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(theme.solid)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_draft(target.clone(), side, line, window, cx);
        }))
        .child(
            crate::icons::icon(crate::icons::PLUS)
                .size(px(11.0))
                .text_color(theme.on_solid),
        )
        .into_any_element()
}

fn render_comment_card(comment: &DiffComment, theme: &Theme, cx: &Context<Changes>) -> AnyElement {
    let group: SharedString = format!("cmt-card-{}", comment.id).into();
    let id = comment.id.clone();
    div()
        .group(group.clone())
        .h(px(comments::card_height(&comment.body)))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.05))
        // A bar, not a border: it must match ACCENT_BAR_WIDTH exactly or the
        // card's edge steps in and out of the column.
        .child(comment_accent_bar(theme.solid.opacity(0.35)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(comments::CARD_PAD_V / 2.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(comment.location())),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cmt-remove-{}", comment.id)))
                                .flex_none()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .opacity(0.0)
                                .group_hover(group, |s| s.opacity(1.0))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove_comment(&id, cx)),
                                )
                                .child(
                                    crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        // Height is analytic, so an over-long body clips
                        // inside the card rather than past the fold height.
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .line_height(px(comments::CARD_LINE_HEIGHT))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(comment.body.clone())),
                ),
        )
        .into_any_element()
}

fn comment_accent_bar(color: gpui::Hsla) -> gpui::Div {
    div().w(px(ACCENT_BAR_WIDTH)).h_full().flex_none().bg(color)
}

/// Mirrors [`DiffComment::cite_path`] for the not-yet-staged note.
fn draft_cite_path(draft: &CommentDraft) -> &str {
    match draft.side {
        CommentSide::Old => draft.old_path.as_deref().unwrap_or(&draft.path),
        CommentSide::New => &draft.path,
    }
}

/// Fixed height, so an open draft never fights the fold tween.
fn render_comment_draft(
    path: &str,
    line: u32,
    input: Entity<ComposerInput>,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    div()
        .h(px(comments::DRAFT_CARD_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.08))
        .child(comment_accent_bar(theme.solid.opacity(0.7)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(10.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("{path}:{line}"))),
                        ),
                )
                .child(
                    div()
                        .h(px(46.0))
                        .flex_none()
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .child(input.into_any_element()),
                )
                .child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(6.0))
                        .child(
                            comment_action("cmt-cancel", "Cancel", false, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                        )
                        .child(
                            comment_action("cmt-commit", "Comment", true, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.commit_draft(cx))),
                        ),
                ),
        )
        .into_any_element()
}

fn comment_action(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |el| el.bg(theme.solid).text_color(theme.on_solid))
        .when(!primary, |el| {
            el.text_color(motion::hover_blend(id, theme.text_muted, theme.text))
                .bg(motion::hover_blend(
                    id,
                    gpui::transparent_black(),
                    theme.element_hover,
                ))
                .on_hover(motion::hover_listener(id))
        })
        .child(SharedString::from(label))
}

/// The expanded body of one file section: notices, hunk headers, +/-/context
/// lines with a coloured accent bar, dual line-number gutters, a marker
/// column, and paint-only syntax runs (holt checkout-diff-sidebar).
/// Shared with the transcript's tool-diff detail blocks — the same component
/// renders a checkout diff section and an inline ACP tool diff. (The changes
/// pane itself virtualizes these rows individually; this stacked form serves
/// the transcript and the fold tween's clipped stand-in.)
/// Full-document old/new highlighting for tool and checkout diffs.
pub(crate) fn render_file_body_with_syntax(
    file: &FileDiff,
    highlights: Option<Arc<DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let gutter_px = gutter_width(file);
    for notice in file_notices(file) {
        children.push(notice_row(notice, theme));
    }
    for hunk in &file.hunks {
        children.push(hunk_header_row(&hunk.header, theme));
        for line in &hunk.lines {
            let spans = highlights
                .as_deref()
                .map(|highlights| highlights.spans(line))
                .unwrap_or(&[]);
            children.push(diff_line_row(line, spans, theme, gutter_px));
        }
    }
    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

/// Build only rows that start above `max_px` so the fold tween's stand-in
/// never materializes lines its clip cannot reveal.
fn render_file_body_upto(
    file: &FileDiff,
    highlight: Option<Arc<DiffHighlights>>,
    theme: &Theme,
    max_px: f32,
    mode: DiffMode,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let mut y = 0.0f32;
    let gutter_px = gutter_width(file);
    let spans_for = |line: &DiffLine| {
        highlight
            .as_deref()
            .map(|highlights| highlights.spans(line))
            .unwrap_or(&[])
    };

    'build: {
        for notice in file_notices(file) {
            if y >= max_px {
                break 'build;
            }
            children.push(notice_row(notice, theme));
            y += NOTICE_HEIGHT;
        }
        for hunk in &file.hunks {
            if y >= max_px {
                break 'build;
            }
            children.push(hunk_header_row(&hunk.header, theme));
            y += HUNK_HEADER_HEIGHT;
            match mode {
                DiffMode::Unified => {
                    for line in &hunk.lines {
                        if y >= max_px {
                            break 'build;
                        }
                        children.push(diff_line_row(line, spans_for(line), theme, gutter_px));
                        y += DIFF_LINE_HEIGHT;
                    }
                }
                DiffMode::Split => {
                    // Pair only what the clip can still reveal: the unified
                    // arm breaks out of a lazy walk, so the split arm must not
                    // materialize the whole hunk first.
                    let budget = ((max_px - y) / DIFF_LINE_HEIGHT).ceil().max(0.0) as usize;
                    for (left, right) in split_pairs_upto(&hunk.lines, budget) {
                        if y >= max_px {
                            break 'build;
                        }
                        let line_at =
                            |slot: Option<u32>| slot.and_then(|slot| hunk.lines.get(slot as usize));
                        let cell = |line: Option<&DiffLine>, old: bool| match line {
                            Some(line) => split_line_cell(
                                line,
                                if old { line.old_no } else { line.new_no },
                                line_runs(line, highlight.as_deref(), theme),
                                theme,
                                gutter_px,
                            )
                            .into_any_element(),
                            None => split_filler().into_any_element(),
                        };
                        let (left, right) = (line_at(left), line_at(right));
                        let marker = [left, right]
                            .into_iter()
                            .flatten()
                            .find(|line| line.kind == LineKind::Meta);
                        children.push(match marker {
                            Some(line) => meta_line_row(
                                &line.text,
                                theme,
                                2.0 * (ACCENT_BAR_WIDTH + gutter_px),
                            ),
                            None => {
                                split_row(cell(left, true), cell(right, false)).into_any_element()
                            }
                        });
                        y += DIFF_LINE_HEIGHT;
                    }
                }
            }
        }
    }

    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

impl Render for Changes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.scope == DiffScope::History {
            let history = self.history_pane(cx);
            history.update(cx, |history, cx| history.ensure_loaded(cx));
            return div().size_full().child(history).into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let active = self.active_diff(cx);
        let scope = self.scope;
        let base = self.base_ref.clone();
        // With no session selected (new-chat canvas) there is nothing to
        // prepare — show the quiet empty state, not an endless spinner.
        let no_chat = self.state.read(cx).selected_chat_row().is_none();
        let phase = if no_chat {
            DiffPhase::Clean
        } else {
            diff_phase(active.as_ref())
        };
        let error = self.error.clone();
        // Scoped fetch failures replace the content area. "no turn recorded"
        // is the expected pre-first-turn state, not an error; "unknown
        // method" is version skew — the connected engine predates
        // GetCheckoutDiff (a still-running daemon after an app update) — say
        // that instead of leaking the raw RPC error (user report).
        let scoped_notice: Option<(SharedString, bool)> = (!no_chat
            && scope != DiffScope::WorkingTree)
            .then(|| self.scoped_error.clone())
            .flatten()
            .map(|message| {
                if message.contains("no turn recorded") {
                    (
                        SharedString::from("No turn recorded yet — send a message first"),
                        false,
                    )
                } else if message.contains("unknown method") {
                    (
                        SharedString::from(
                            "This Holt build doesn't support branch and turn diffs yet",
                        ),
                        false,
                    )
                } else {
                    (message, true)
                }
            });

        let content: AnyElement = if let Some((message, warn)) = scoped_notice {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(Theme::SPACE_LG))
                .text_size(px(12.0))
                .text_color(if warn {
                    theme.warning.opacity(0.85)
                } else {
                    theme.text_faint
                })
                .child(message)
                .into_any_element()
        } else {
            match phase {
                DiffPhase::Preparing => div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(crate::loaders::gradient_spinner(
                        "changes-preparing",
                        &theme,
                        3.0,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("Preparing diff…")),
                    )
                    .into_any_element(),
                DiffPhase::Clean => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(clean_message(scope, base.as_deref())))
                    .into_any_element(),
                DiffPhase::List => {
                    if self.parsed.is_some() {
                        let sticky_header = self.render_sticky_file_header(&theme, cx);
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .children(self.render_header_strip(&theme))
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(
                                        list(self.list.clone(), cx.processor(Self::render_row))
                                            .size_full()
                                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                                    )
                                    .when_some(sticky_header, |el, header| el.child(header)),
                            )
                            .into_any_element()
                    } else {
                        // Diff known, parse still running.
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(crate::loaders::gradient_spinner(
                                "changes-parsing",
                                &theme,
                                3.0,
                                cx.entity_id(),
                                cx,
                            ))
                            .into_any_element()
                    }
                }
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            // Changes is a code-adjacent surface: chrome stays Geist while
            // paths, hunks, gutters, and source runs keep their mono overrides.
            .font_family(theme.font_sans_fixed.clone())
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(content)
            .into_any_element()
    }
}
