//! The right-pane Git panel (tickets 03, 06, 07): a per-chat surface with two
//! internal tabs. Status renders the live working-tree status stream as
//! three flat, path-sorted sections — Staged, Unstaged, Untracked — a
//! read-only view of the real index that never drifts from it, including
//! after external terminal git operations. The header carries the
//! working-tree diff's total +/- counts (whose file set is exactly the
//! union of the three sections) and a View Diff action; a row click opens
//! the Changes surface scrolled to that file. History hosts the existing
//! commit-graph entity, refreshing when the tab becomes visible. The
//! panel owns one `WatchWorkspaceGitStatus` subscription and one
//! `WatchCheckoutDiffs` subscription from open to close.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, SharedString, Subscription, Task, div, px};
use holt_proto::{
    Chat, CheckoutDiff, GitHistoryCommit, WorkspaceGitStatus, WorkspaceGitStatusEntry,
    WorkspaceGitStatusKind,
};
use holt_rpc::methods;

use crate::changes::apply_diff_frame;
use crate::files::tree::{marker_color, marker_parts};
use crate::history::GitHistory;
use crate::icons::{self, icon};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// The panel's internal tabs — plain view state, not right-pane surface
/// tabs (ticket 07): one panel, two faces of the same checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitPanelTab {
    #[default]
    Status,
    History,
}

impl GitPanelTab {
    fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::History => "History",
        }
    }

    /// The tab-switch element id.
    fn id_tag(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::History => "history",
        }
    }
}

/// Events the host (the right pane's surface strip) listens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitPanelEvent {
    /// Open (or focus) this panel's companion Changes surface on the
    /// working-tree scope, scrolled to the file when one is given.
    ViewDiff { path: Option<String> },
    /// A History commit row was clicked — open it as its own pinned
    /// Changes diff tab (the same routing as the Changes pane's History
    /// scope).
    OpenCommit(GitHistoryCommit),
}

impl gpui::EventEmitter<GitPanelEvent> for GitPanel {}

/// The section a status row belongs to: the index's Staged side versus
/// what the worktree still owes it (Unstaged and Untracked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSection {
    Staged,
    Unstaged,
    Untracked,
}

impl StatusSection {
    fn label(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::Untracked => "Untracked",
        }
    }

    /// The row-element id fragment (`git-status-row-<tag>-<ix>`).
    fn id_tag(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

/// One renderable status row: the entry's path, the SECTION SIDE's kind
/// (a both-sides-modified file carries `Modified` in Staged from its index
/// side and again in Unstaged from its worktree side), and the display
/// flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub section: StatusSection,
    pub path: String,
    pub kind: WorkspaceGitStatusKind,
    /// Whole-directory entry (a collapsed untracked directory): renders
    /// with a folder icon and a trailing slash.
    pub is_dir: bool,
    /// Unmerged path: the row is marked with the conflict hint.
    pub conflicted: bool,
}

/// The three sections of the Status tab, each flat and path-sorted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusSections {
    pub staged: Vec<StatusRow>,
    pub unstaged: Vec<StatusRow>,
    pub untracked: Vec<StatusRow>,
}

impl StatusSections {
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }
}

/// Assign one status snapshot to the three sections (pure — the panel's
/// tested seam). Ignored entries never render. A file with both staged and
/// unstaged changes appears once in each section; an untracked path lands
/// in Untracked regardless of anything else.
pub fn assign_sections(entries: &[WorkspaceGitStatusEntry]) -> StatusSections {
    let mut sections = StatusSections::default();
    for entry in entries {
        if entry.kind == WorkspaceGitStatusKind::Ignored {
            continue;
        }
        // The engine never reports a tracked entry with both sides clean;
        // such a row would vanish from every section.
        debug_assert!(
            entry.index.is_some() || entry.worktree.is_some(),
            "non-ignored status entry carries no porcelain side: {}",
            entry.path
        );
        let conflicted = entry.kind == WorkspaceGitStatusKind::Conflicted
            || entry.index == Some(WorkspaceGitStatusKind::Conflicted)
            || entry.worktree == Some(WorkspaceGitStatusKind::Conflicted);
        if let Some(kind) = entry.index {
            sections.staged.push(StatusRow {
                section: StatusSection::Staged,
                path: entry.path.clone(),
                kind,
                is_dir: entry.is_dir,
                conflicted,
            });
        }
        if let Some(kind) = entry.worktree {
            let row = StatusRow {
                section: if kind == WorkspaceGitStatusKind::Untracked {
                    StatusSection::Untracked
                } else {
                    StatusSection::Unstaged
                },
                path: entry.path.clone(),
                kind,
                is_dir: entry.is_dir,
                conflicted,
            };
            match row.section {
                StatusSection::Untracked => sections.untracked.push(row),
                _ => sections.unstaged.push(row),
            }
        }
    }
    let by_path = |a: &StatusRow, b: &StatusRow| a.path.cmp(&b.path);
    sections.staged.sort_by(by_path);
    sections.unstaged.sort_by(by_path);
    sections.untracked.sort_by(by_path);
    sections
}

/// The working-tree diff whose totals the header shows (ticket 06): the
/// panel's own chat resolved exactly like the Changes pane resolves its
/// watch — checkout id, then device + cwd, then cwd — so the two surfaces
/// can never disagree about which checkout they describe; with no chat
/// (the space / new-chat canvas) the diff whose cwd matches the status
/// stream's own workdir spelling. Its file set is the union of the three
/// status sections by construction (worktree ∪ index vs HEAD), so the
/// totals always match the list below them — with the known inherited
/// asymmetry that a RENAME lists as delete + untracked in the panel while
/// the diff folds it into one rename entry (status reporting disables
/// rename detection, diff capture enables it); documented, not fixed.
pub fn resolve_panel_diff<'a>(
    diffs: &'a [CheckoutDiff],
    chat: Option<&Chat>,
    workdir: Option<&str>,
) -> Option<&'a CheckoutDiff> {
    if let Some(diff) = chat.and_then(|chat| crate::changes::resolve_diff(diffs, chat)) {
        return Some(diff);
    }
    let workdir = workdir?;
    diffs.iter().find(|d| d.cwd == workdir)
}

/// The panel's view of the status stream. Kept cx-free so frame handling is
/// unit-testable without an app: the watch task applies frames here and just
/// notifies.
#[derive(Debug, Default)]
struct StatusView {
    /// The canonical workdir the row paths are relative to (display only).
    workdir: Option<String>,
    /// False until the first frame lands — the Loading state.
    frame_seen: bool,
    /// The watched root is not inside a git work tree (`workdir: null`):
    /// a banner state, never an error.
    not_git: bool,
    /// Rows from the last GOOD frame — an error frame never clears them.
    sections: StatusSections,
    /// Banner text: a git read failure or a stream interruption. Renders
    /// over the last good frame (the Changes pane's watch-error pattern).
    error: Option<String>,
}

impl StatusView {
    fn apply(&mut self, frame: WorkspaceGitStatus) {
        self.frame_seen = true;
        let Some(workdir) = frame.workdir else {
            self.not_git = true;
            self.workdir = None;
            self.sections = StatusSections::default();
            self.error = None;
            return;
        };
        self.not_git = false;
        self.workdir = Some(workdir);
        if let Some(error) = frame.error {
            // A read failure keeps the last good frame underneath the banner.
            self.error = Some(error);
            return;
        }
        self.error = None;
        self.sections = assign_sections(&frame.entries);
    }

    /// The stream itself dropped or never subscribed (engine restart): the
    /// retry loop re-arms, the banner rides the last good frame meanwhile.
    fn note_stream_error(&mut self, message: String) {
        self.error = Some(message);
    }
}

pub struct GitPanel {
    state: Entity<AppState>,
    /// RPC selector captured at open: the owning chat, else the space (the
    /// new-chat canvas). The tab is chat-scoped in the strip, so the
    /// selector never needs re-deriving.
    chat_id: Option<String>,
    space_id: Option<String>,
    view: StatusView,
    /// The status watch, kept so its drop cancels the engine-side stream.
    /// `None` until the engine handle exists (the state observer retries).
    watch: Option<Task<()>>,
    /// The working-tree diff watch (the Changes pane's own stream): frames
    /// fold into [`Self::diffs`] and feed the header totals. `None` until
    /// the engine handle exists.
    diff_watch: Option<Task<()>>,
    /// The last diff frame set — every checkout the engine streams, resolved
    /// down to this panel's checkout at render time.
    diffs: Vec<CheckoutDiff>,
    /// Which internal tab is showing (ticket 07).
    tab: GitPanelTab,
    /// The History tab's commit-graph entity — the same one the Changes
    /// pane's History scope hosts, instantiated separately so both
    /// coexist. Created lazily on first show; owns its data and rendering.
    history: Option<Entity<GitHistory>>,
    /// Holds the history entity's event subscription for the panel's
    /// lifetime.
    history_events: Option<Subscription>,
}

impl GitPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let (chat_id, space_id) = {
            let state = state.read(cx);
            match state.selected_chat.clone() {
                Some(chat_id) => (Some(chat_id), None),
                None => (None, state.selected_space.clone()),
            }
        };
        let mut panel = Self {
            state,
            chat_id,
            space_id,
            view: StatusView::default(),
            watch: None,
            diff_watch: None,
            diffs: Vec::new(),
            tab: GitPanelTab::default(),
            history: None,
            history_events: None,
        };
        panel.ensure_watch(cx);
        // The engine can attach AFTER the panel opens (boot ordering): retry
        // on state changes until the watch starts, then this stays a no-op.
        cx.observe(&panel.state, |this, _, cx| this.ensure_watch(cx))
            .detach();
        panel
    }

    /// Start the panel's streams once an engine exists: the status watch
    /// (which also needs a chat/space selector) and the diff watch.
    /// Idempotent per stream — an established watch is never duplicated.
    fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if self.watch.is_some() && self.diff_watch.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.watch.is_none()
            && let Some(params) = self.status_params()
        {
            self.watch = Some(Self::spawn_watch(engine.clone(), params, cx));
        }
        if self.diff_watch.is_none() {
            self.diff_watch = Some(Self::spawn_diff_watch(engine, cx));
        }
    }

    /// The status watch's selector params, when a chat or a space exists.
    fn status_params(&self) -> Option<serde_json::Value> {
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &self.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &self.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        } else {
            return None;
        }
        Some(serde_json::Value::Object(params))
    }

    /// The Changes pane's resubscribe loop: frames apply to the view; a
    /// ended/failed stream raises the banner and retries after 2s, never
    /// clearing the rows underneath.
    fn spawn_watch(
        engine: EngineHandle,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let subscribed = engine
                    .client()
                    .subscribe(
                        holt_rpc::methods::WATCH_WORKSPACE_GIT_STATUS,
                        params.clone(),
                    )
                    .await;
                match subscribed {
                    Ok(mut frames) => {
                        while let Some(value) = frames.recv().await {
                            let Ok(frame) = serde_json::from_value::<WorkspaceGitStatus>(value)
                            else {
                                continue;
                            };
                            let alive = this.update(cx, |panel, cx| {
                                panel.view.apply(frame);
                                cx.notify();
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        // Stream ended (engine restart / reconnect): banner
                        // over the last good frame, then retry.
                        let alive = this.update(cx, |panel, cx| {
                            panel
                                .view
                                .note_stream_error("Status stream interrupted — retrying".into());
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let alive = this.update(cx, |panel, cx| {
                            panel
                                .view
                                .note_stream_error(format!("Status watch unavailable: {err}"));
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    /// The Changes pane's diff-watch loop, narrowed to the panel's need:
    /// frames fold into [`Self::diffs`] (the same `apply_diff_frame`) to
    /// feed the header totals. No parse and no error banner — an absent or
    /// interrupted stream just means the totals are not shown yet, and the
    /// rows below never depend on it.
    fn spawn_diff_watch(engine: EngineHandle, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let subscribed = engine
                    .client()
                    .subscribe(methods::WATCH_CHECKOUT_DIFFS, serde_json::json!({}))
                    .await;
                if let Ok(mut frames) = subscribed {
                    while let Some(value) = frames.recv().await {
                        let alive = this.update(cx, |panel, cx| {
                            if apply_diff_frame(&mut panel.diffs, value) {
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    /// The History tab's commit-graph entity, created on first show (the
    /// Changes pane's own `history_pane` recipe): it owns its data and
    /// rendering, and its commit clicks re-emit as
    /// [`GitPanelEvent::OpenCommit`] for the surface strip to route — the
    /// same pinned-diff tab the Changes pane's History scope opens.
    fn ensure_history(&mut self, cx: &mut Context<Self>) -> Entity<GitHistory> {
        if let Some(history) = &self.history {
            return history.clone();
        }
        let history = cx.new(|cx| GitHistory::new(self.state.clone(), cx));
        self.history_events = Some(cx.subscribe(&history, |_this: &mut Self, _, event, cx| {
            if let crate::history::GitHistoryEvent::OpenCommit(commit) = event {
                cx.emit(GitPanelEvent::OpenCommit(commit.clone()));
            }
        }));
        self.history = Some(history.clone());
        history
    }

    /// Switch the internal tab. Becoming visible is History's refresh
    /// trigger: the entity is created (and loads) on first show, then
    /// force-reloads on every later visit. Branch switches made elsewhere
    /// while the panel stays open stay stale until the trigger fires, by
    /// design — the status payload deliberately carries no head sha to key
    /// a smarter invalidation.
    fn select_tab(&mut self, tab: GitPanelTab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.ensure_visible(cx);
        cx.notify();
    }

    /// The panel (or its History tab) became the visible surface — the
    /// shell's activation hook. When History is the showing tab, its
    /// visibility refresh fires; the Status tab needs nothing (its watch
    /// runs from open to close).
    pub fn ensure_visible(&mut self, cx: &mut Context<Self>) {
        if self.tab == GitPanelTab::History {
            let history = self.ensure_history(cx);
            history.update(cx, |history, cx| history.ensure_current(cx));
        }
    }

    fn render_section(
        &self,
        theme: &Theme,
        section: StatusSection,
        rows: &[StatusRow],
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child(section.label()),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(format!("{}", rows.len()))),
                    ),
            )
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, row)| self.render_row(theme, ix, row, cx)),
            )
            .into_any_element()
    }

    /// One status row: kind badge, path. Clicking anywhere on the row opens
    /// the Changes surface scrolled to this file (ticket 06). The row's
    /// visible state only ever follows the stream, never a local guess;
    /// rows carry kind badges only — no per-file statistics.
    fn render_row(
        &self,
        theme: &Theme,
        ix: usize,
        row: &StatusRow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut path = row.path.clone();
        if row.is_dir {
            path.push('/');
        }
        let parts = marker_parts(row.kind);
        let badge = parts.as_ref().map(|(letter, _)| {
            div()
                .flex_none()
                .w(px(12.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(crate::typography::ui_rems(10.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(marker_color(row.kind, theme).opacity(0.9))
                .child(*letter)
        });
        let label = parts.map(|(_, label)| label);
        // The row itself is the click-to-diff target — conflicted rows
        // included (viewing a conflicted file is exactly when the diff
        // helps); the directory row reveals its first file in the diff.
        let row_path = row.path.clone();
        let el = div()
            .id(SharedString::from(format!(
                "git-status-row-{}-{ix}",
                row.section.id_tag()
            )))
            .w_full()
            .h(px(28.0))
            .pl(px(6.0))
            .pr(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .children(badge)
            .when(row.is_dir, |el| {
                el.child(
                    icon(icons::FOLDER)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.8)),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text.opacity(0.9))
                    .child(path),
            )
            // Inline hint: conflicted rows are marked.
            .when(row.conflicted, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.danger.opacity(0.9))
                        .child("conflict"),
                )
            })
            .cursor_pointer()
            .hover(|state| state.bg(crate::theme::wash(0.04)))
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                cx.emit(GitPanelEvent::ViewDiff {
                    path: Some(row_path.clone()),
                });
            }));
        el.when_some(label, |el, label| {
            el.tooltip(move |_, cx| {
                cx.new(|_| crate::image_viewer::ViewerTooltip(label.clone()))
                    .into()
            })
        })
        .into_any_element()
    }

    /// The slim header: the internal Status | History tab switch, the
    /// checkout this panel describes (workdir), the working-tree diff's
    /// total counts in the Changes header's own spelling (`+N −M`, mono,
    /// add/del colors), and the View Diff action (ticket 06) — the totals
    /// and the diff action belong to the Status tab's list. Totals hide
    /// while no diff frame has landed — they are commentary on the list,
    /// never a gate on it.
    fn render_header(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let chat = self
            .chat_id
            .as_deref()
            .and_then(|id| self.state.read(cx).chat_row(id));
        let totals = resolve_panel_diff(&self.diffs, chat, self.view.workdir.as_deref())
            .map(|diff| (diff.additions, diff.deletions));
        let on_status = self.tab == GitPanelTab::Status;
        div()
            .flex_none()
            .h(px(36.0))
            .pl(px(8.0))
            .pr(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .child(Self::tab_chip(theme, self.tab, GitPanelTab::Status, cx))
                    .child(Self::tab_chip(theme, self.tab, GitPanelTab::History, cx)),
            )
            .children(self.view.workdir.clone().map(|workdir| {
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(workdir))
            }))
            .when_some(
                on_status.then_some(totals).flatten(),
                |el, (additions, deletions)| {
                    el.child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .text_color(theme.diff_add)
                                    .child(SharedString::from(format!("+{additions}"))),
                            )
                            .child(
                                div()
                                    .text_color(theme.diff_del)
                                    .child(SharedString::from(format!("−{deletions}"))),
                            ),
                    )
                },
            )
            .when(on_status, |el| el.child(Self::view_diff_button(theme, cx)))
            .into_any_element()
    }

    /// One internal-tab chip: the active tab carries a wash and full
    /// weight; the inactive one is a quiet switch.
    fn tab_chip(
        theme: &Theme,
        current: GitPanelTab,
        tab: GitPanelTab,
        cx: &Context<Self>,
    ) -> AnyElement {
        let active = current == tab;
        let mut chip = div()
            .id(SharedString::from(format!("git-tab-{}", tab.id_tag())))
            .flex_none()
            .h(px(22.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(11.5))
            .font_weight(if active {
                gpui::FontWeight::SEMIBOLD
            } else {
                gpui::FontWeight::MEDIUM
            })
            .text_color(if active { theme.text } else { theme.text_muted });
        if active {
            chip = chip.bg(crate::theme::wash(0.06));
        } else {
            chip = chip
                .cursor_pointer()
                .hover(|state| state.bg(crate::theme::wash(0.05)).text_color(theme.text))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_tab(tab, cx);
                }));
        }
        chip.child(tab.label()).into_any_element()
    }

    /// The header's View Diff action: opens (or focuses) the panel's
    /// companion Changes surface on the working-tree scope.
    fn view_diff_button(theme: &Theme, cx: &Context<Self>) -> AnyElement {
        div()
            .id("git-view-diff")
            .flex_none()
            .h(px(20.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(px(5.0))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|state| state.bg(crate::theme::wash(0.06)).text_color(theme.text))
            .on_click(cx.listener(|_this, _event, _window, cx| {
                cx.emit(GitPanelEvent::ViewDiff { path: None });
            }))
            .child("View Diff")
            .into_any_element()
    }

    /// The banner shared by the read-failure and not-a-repository states:
    /// an 11px warning line over the content (the Changes pane's
    /// watch-error pattern).
    fn banner(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
        div()
            .flex_none()
            .px(px(Theme::SPACE_MD))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .text_color(theme.warning)
            .child(message.into())
    }

    fn render_centered(theme: &Theme, icon_path: &'static str, message: &str) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icon(icon_path)
                            .size(px(18.0))
                            .text_color(theme.text_muted.opacity(0.6)),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(message.to_string())),
                    ),
            )
            .into_any_element()
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let on_status = self.tab == GitPanelTab::Status;
        let body: AnyElement = if !on_status {
            // The History tab: the commit-graph entity, full bleed — it owns
            // its data, list, empty states ("No commits yet" included), and
            // even its own render-time `ensure_loaded`. The visibility
            // reload itself happened in `select_tab` / `ensure_visible`.
            let history = self.ensure_history(cx);
            div()
                .size_full()
                .flex_1()
                .min_h_0()
                .child(history)
                .into_any_element()
        } else if !self.view.frame_seen {
            Self::render_centered(&theme, icons::GIT_BRANCH, "Reading git status…")
        } else if self.view.not_git {
            Self::render_centered(
                &theme,
                icons::GIT_BRANCH,
                "No repository here — status needs a git working tree",
            )
        } else if self.view.sections.is_empty() && self.view.error.is_none() {
            Self::render_centered(&theme, icons::CHECK, "Working tree clean")
        } else {
            let mut sections = div().flex().flex_col().pb(px(12.0));
            if !self.view.sections.staged.is_empty() {
                sections = sections.child(self.render_section(
                    &theme,
                    StatusSection::Staged,
                    &self.view.sections.staged,
                    cx,
                ));
            }
            if !self.view.sections.unstaged.is_empty() {
                sections = sections.child(self.render_section(
                    &theme,
                    StatusSection::Unstaged,
                    &self.view.sections.unstaged,
                    cx,
                ));
            }
            if !self.view.sections.untracked.is_empty() {
                sections = sections.child(self.render_section(
                    &theme,
                    StatusSection::Untracked,
                    &self.view.sections.untracked,
                    cx,
                ));
            }
            div()
                .id("git-panel-status")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(sections)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            // The slim header: the Status | History switch, the checkout
            // this panel describes, the working-tree diff's total +/-
            // counts, and View Diff.
            .child(self.render_header(&theme, cx))
            // A read failure / stream interruption banners over the last
            // good frame (the Changes pane's watch-error pattern); a non-git
            // root banners with no rows rendered. Both tabs — they describe
            // the checkout, not the status list.
            .when_some(self.view.error.clone(), |el, message| {
                el.child(Self::banner(&theme, message))
            })
            .when(self.view.not_git, |el| {
                el.child(Self::banner(&theme, "Not a git repository"))
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::WorkspaceGitStatusKind as Kind;

    fn entry(
        path: &str,
        kind: Kind,
        index: Option<Kind>,
        worktree: Option<Kind>,
        is_dir: bool,
    ) -> WorkspaceGitStatusEntry {
        WorkspaceGitStatusEntry {
            path: path.into(),
            kind,
            index,
            worktree,
            is_dir,
        }
    }

    fn frame(
        workdir: Option<&str>,
        entries: Vec<WorkspaceGitStatusEntry>,
        error: Option<&str>,
    ) -> WorkspaceGitStatus {
        WorkspaceGitStatus {
            workdir: workdir.map(str::to_string),
            entries,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn staged_side_lands_in_staged() {
        let sections = assign_sections(&[entry(
            "src/new.rs",
            Kind::Added,
            Some(Kind::Added),
            None,
            false,
        )]);
        assert_eq!(sections.staged.len(), 1);
        assert_eq!(sections.staged[0].kind, Kind::Added);
        assert_eq!(sections.staged[0].path, "src/new.rs");
        assert!(sections.unstaged.is_empty());
        assert!(sections.untracked.is_empty());
    }

    #[test]
    fn worktree_side_lands_in_unstaged() {
        let sections = assign_sections(&[entry(
            "src/lib.rs",
            Kind::Modified,
            None,
            Some(Kind::Modified),
            false,
        )]);
        assert!(sections.staged.is_empty());
        assert_eq!(sections.unstaged.len(), 1);
        assert_eq!(sections.unstaged[0].kind, Kind::Modified);
        assert!(sections.untracked.is_empty());
    }

    #[test]
    fn a_both_sides_modified_file_appears_once_in_each_section() {
        let sections = assign_sections(&[entry(
            "src/lib.rs",
            Kind::Modified,
            Some(Kind::Modified),
            Some(Kind::Modified),
            false,
        )]);
        assert_eq!(sections.staged.len(), 1);
        assert_eq!(sections.unstaged.len(), 1);
        assert_eq!(sections.staged[0].path, "src/lib.rs");
        assert_eq!(sections.unstaged[0].path, "src/lib.rs");
        assert!(sections.untracked.is_empty());
    }

    #[test]
    fn untracked_entries_land_in_untracked_and_directories_keep_the_flag() {
        let sections = assign_sections(&[
            entry(
                "notes.md",
                Kind::Untracked,
                None,
                Some(Kind::Untracked),
                false,
            ),
            entry(
                "prototype",
                Kind::Untracked,
                None,
                Some(Kind::Untracked),
                true,
            ),
        ]);
        assert!(sections.staged.is_empty());
        assert!(sections.unstaged.is_empty());
        assert_eq!(sections.untracked.len(), 2);
        assert_eq!(sections.untracked[0].path, "notes.md");
        assert!(!sections.untracked[0].is_dir);
        assert_eq!(sections.untracked[1].path, "prototype");
        assert!(sections.untracked[1].is_dir);
    }

    #[test]
    fn conflicted_paths_are_marked_in_both_sections() {
        let sections = assign_sections(&[entry(
            "src/merge.rs",
            Kind::Conflicted,
            Some(Kind::Conflicted),
            Some(Kind::Conflicted),
            false,
        )]);
        assert_eq!(sections.staged.len(), 1);
        assert_eq!(sections.unstaged.len(), 1);
        assert!(sections.staged[0].conflicted);
        assert!(sections.unstaged[0].conflicted);
    }

    #[test]
    fn ignored_entries_never_render() {
        let sections = assign_sections(&[
            entry("debug.log", Kind::Ignored, None, None, false),
            entry("target", Kind::Ignored, None, None, true),
        ]);
        assert!(sections.is_empty());
    }

    #[test]
    fn sections_are_path_sorted_regardless_of_frame_order() {
        let sections = assign_sections(&[
            entry("z.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry("a.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry("m.rs", Kind::Modified, None, Some(Kind::Modified), false),
            entry("b.rs", Kind::Modified, None, Some(Kind::Modified), false),
        ]);
        let staged: Vec<&str> = sections.staged.iter().map(|r| r.path.as_str()).collect();
        let unstaged: Vec<&str> = sections.unstaged.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(staged, ["a.rs", "z.rs"]);
        assert_eq!(unstaged, ["b.rs", "m.rs"]);
    }

    #[test]
    fn an_error_frame_keeps_the_last_good_rows_under_the_banner() {
        let mut view = StatusView::default();
        view.apply(frame(
            Some("/repo"),
            vec![entry(
                "a.rs",
                Kind::Modified,
                Some(Kind::Modified),
                None,
                false,
            )],
            None,
        ));
        assert_eq!(view.sections.staged.len(), 1);

        view.apply(frame(Some("/repo"), Vec::new(), Some("corrupt index")));
        assert_eq!(view.error.as_deref(), Some("corrupt index"));
        assert_eq!(
            view.sections.staged.len(),
            1,
            "the error frame never clears the last good rows"
        );

        // The next good frame clears the banner and refreshes the rows.
        view.apply(frame(Some("/repo"), Vec::new(), None));
        assert_eq!(view.error, None);
        assert!(view.sections.is_empty());
    }

    #[test]
    fn a_non_git_root_is_a_banner_state_never_an_error() {
        let mut view = StatusView::default();
        view.apply(frame(
            Some("/repo"),
            vec![entry(
                "a.rs",
                Kind::Modified,
                Some(Kind::Modified),
                None,
                false,
            )],
            None,
        ));
        view.apply(frame(None, Vec::new(), None));
        assert!(view.not_git);
        assert!(view.sections.is_empty());
        assert_eq!(view.error, None);
        assert!(view.frame_seen);
        // The header must not keep showing the vanished checkout's path.
        assert_eq!(view.workdir, None);
    }

    #[test]
    fn a_clean_tree_is_empty_without_an_error() {
        let mut view = StatusView::default();
        view.apply(frame(Some("/repo"), Vec::new(), None));
        assert!(view.frame_seen);
        assert!(!view.not_git);
        assert!(view.sections.is_empty());
        assert_eq!(view.error, None);
        assert_eq!(view.workdir.as_deref(), Some("/repo"));
    }

    #[test]
    fn a_stream_interruption_banners_without_touching_rows() {
        let mut view = StatusView::default();
        view.apply(frame(
            Some("/repo"),
            vec![entry(
                "a.rs",
                Kind::Modified,
                Some(Kind::Modified),
                None,
                false,
            )],
            None,
        ));
        view.note_stream_error("Status stream interrupted — retrying".into());
        assert!(view.error.is_some());
        assert_eq!(view.sections.staged.len(), 1);
    }

    fn checkout_diff(checkout_id: &str, cwd: &str, additions: u32, deletions: u32) -> CheckoutDiff {
        CheckoutDiff {
            checkout_id: checkout_id.into(),
            device_id: "dev".into(),
            cwd: cwd.into(),
            patch: String::new(),
            files: Vec::new(),
            additions,
            deletions,
            truncated: false,
            checksum: format!("sum-{checkout_id}"),
            updated_at: chrono::Utc::now(),
        }
    }

    fn chat_row(id: &str, cwd: Option<&str>, checkout_id: Option<&str>) -> Chat {
        use holt_proto::TitleSource;
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            title_source: TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: cwd.map(str::to_string),
            branch: None,
            checkout_id: checkout_id.map(str::to_string),
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            worktree: None,
            provider_mode: false,
        }
    }

    #[test]
    fn header_totals_resolve_the_panels_own_chat_like_the_changes_pane() {
        let diffs = vec![
            checkout_diff("other", "/elsewhere", 1, 2),
            checkout_diff("co-1", "/repo", 30, 7),
        ];
        // Checkout id wins outright.
        let chat = chat_row("c1", Some("/somewhere-else"), Some("co-1"));
        assert_eq!(
            resolve_panel_diff(&diffs, Some(&chat), None).map(|d| (d.additions, d.deletions)),
            Some((30, 7))
        );
        // Without a checkout id, the Changes pane's cwd precedence applies.
        let chat = chat_row("c1", Some("/elsewhere"), None);
        assert_eq!(
            resolve_panel_diff(&diffs, Some(&chat), None).map(|d| d.cwd.as_str()),
            Some("/elsewhere")
        );
        // A chat on an unknown checkout falls through to the workdir
        // spelling — never to some other checkout's diff.
        let chat = chat_row("c1", None, Some("co-gone"));
        assert_eq!(resolve_panel_diff(&diffs, Some(&chat), None), None);
    }

    #[test]
    fn header_totals_fall_back_to_the_status_workdir_without_a_chat() {
        let diffs = vec![
            checkout_diff("other", "/elsewhere", 1, 2),
            checkout_diff("co-1", "/repo", 30, 7),
        ];
        assert_eq!(
            resolve_panel_diff(&diffs, None, Some("/repo")).map(|d| (d.additions, d.deletions)),
            Some((30, 7))
        );
        assert_eq!(resolve_panel_diff(&diffs, None, Some("/nope")), None);
        assert_eq!(resolve_panel_diff(&diffs, None, None), None);
    }

    #[gpui::test]
    fn the_history_tab_is_lazy_and_its_entity_survives_tab_switches(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Theme::default());
        });
        let state = cx.new(|_| AppState::new());
        let (panel, visual) = cx.add_window_view(|_window, cx| GitPanel::new(state, cx));
        panel.update(&mut *visual, |panel, cx| {
            assert_eq!(panel.tab, GitPanelTab::Status);
            assert!(
                panel.history.is_none(),
                "the graph entity is created lazily"
            );

            panel.select_tab(GitPanelTab::History, cx);
            assert_eq!(panel.tab, GitPanelTab::History);
            assert!(panel.history.is_some(), "first show creates the entity");

            let entity = panel.history.clone().unwrap();
            panel.select_tab(GitPanelTab::Status, cx);
            assert_eq!(panel.tab, GitPanelTab::Status);
            assert!(
                panel.history.as_ref() == Some(&entity),
                "switching away keeps the instance"
            );
            panel.select_tab(GitPanelTab::History, cx);
            assert!(panel.history.as_ref() == Some(&entity));
        });
    }
}
