//! The right-pane Git panel (tickets 03–07): a per-chat surface with two
//! internal tabs. Status renders the live working-tree status stream as
//! three flat, path-sorted sections — Staged, Unstaged, Untracked — with
//! live staging and the commit box. Checking an unstaged/untracked row
//! stages it, unchecking a staged row unstages it, and Stage all /
//! Unstage all move everything in one click. State is purely
//! stream-driven — no optimistic local layer — so the rows can never
//! drift from the real index, including after external terminal git
//! operations; a refused write surfaces the engine's message in an error
//! strip. The commit box reuses the composer input (Enter commits,
//! Shift-Enter breaks the line) with a count-labeled button gated on
//! message ∧ staged ∧ no-conflicts — the engine's own gates (identity,
//! mid-merge) remain the backstop. The header carries the working-tree
//! diff's total +/- counts (whose file set is exactly the union of the
//! three sections) and a View Diff action; a row click opens the Changes
//! surface scrolled to that file. History hosts the existing commit-graph
//! entity, refreshing on exactly two triggers: a successful commit from
//! the panel and the tab becoming visible. The panel owns one
//! `WatchWorkspaceGitStatus` subscription and one `WatchCheckoutDiffs`
//! subscription from open to close.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, SharedString, Subscription, Task, div, px};
use holt_proto::{
    Chat, CheckoutDiff, GitHistoryCommit, WorkspaceGitStatus, WorkspaceGitStatusEntry,
    WorkspaceGitStatusKind,
};
use holt_rpc::methods;

use crate::changes::apply_diff_frame;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::files::tree::{marker_color, marker_parts};
use crate::history::GitHistory;
use crate::icons::{self, icon};
use crate::settings::widgets::{self, CheckboxState};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// How long the committed short sha stays flashed beside the button.
const COMMIT_FLASH: Duration = Duration::from_secs(4);

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

/// The section a status row belongs to. Staged answers "what would the next
/// commit contain?"; Unstaged and Untracked are what the worktree still owes
/// the index.
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
    /// Whole-directory entry (a collapsed untracked directory): staging it
    /// stages everything inside.
    pub is_dir: bool,
    /// Unmerged path: the row is marked and its checkbox disabled.
    pub conflicted: bool,
}

/// The three sections of the Status tab, each flat and path-sorted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusSections {
    pub staged: Vec<StatusRow>,
    pub unstaged: Vec<StatusRow>,
    pub untracked: Vec<StatusRow>,
}

/// One staging write the panel can issue through the engine's write trio
/// (ADR-0022): stage or unstage a batch of repo-relative paths. The paths
/// are stream spellings — the slash-stripped forms the status rows render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingAction {
    Stage(Vec<String>),
    Unstage(Vec<String>),
}

impl StagingAction {
    fn paths(&self) -> &[String] {
        match self {
            Self::Stage(paths) | Self::Unstage(paths) => paths,
        }
    }
}

impl StatusRow {
    /// The write this row's checkbox performs: a Staged row's uncheck
    /// unstages its index side, an Unstaged/Untracked row's check stages
    /// its worktree side (a directory stages recursively, after which its
    /// files arrive as individual Staged rows). Conflicted rows are never
    /// offered an action — the engine refuses them and the row renders
    /// disabled.
    fn staging_action(&self) -> Option<StagingAction> {
        if self.conflicted {
            return None;
        }
        match self.section {
            StatusSection::Staged => Some(StagingAction::Unstage(vec![self.path.clone()])),
            StatusSection::Unstaged | StatusSection::Untracked => {
                Some(StagingAction::Stage(vec![self.path.clone()]))
            }
        }
    }
}

impl StatusSections {
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    /// The paths Stage all would stage: every unstaged and untracked row,
    /// conflicted ones aside (one conflicted path refuses the whole batch
    /// engine-side, and a conflict must never block the clean paths).
    pub fn stage_all_paths(&self) -> Vec<String> {
        self.unstaged
            .iter()
            .chain(&self.untracked)
            .filter(|row| !row.conflicted)
            .map(|row| row.path.clone())
            .collect()
    }

    /// The paths Unstage all would unstage: every staged row, conflicted
    /// ones aside for the same reason.
    pub fn unstage_all_paths(&self) -> Vec<String> {
        self.staged
            .iter()
            .filter(|row| !row.conflicted)
            .map(|row| row.path.clone())
            .collect()
    }

    /// Whether any rendered row is conflicted. A conflict anywhere means
    /// the engine will refuse the commit (a conflicted tree is mid-merge),
    /// so the commit button steps aside no matter which section carries it.
    pub fn has_conflicts(&self) -> bool {
        self.staged
            .iter()
            .chain(&self.unstaged)
            .chain(&self.untracked)
            .any(|row| row.conflicted)
    }
}

/// The commit button's UI gate (ticket 05): a non-blank message ∧ at
/// least one staged path ∧ no conflicted paths anywhere. Presentation
/// only — the engine's own gates (identity, mid-merge, nothing staged)
/// remain the backstop, so this never decides commit correctness, just
/// whether the click is worth offering.
pub fn commit_enabled(message: &str, sections: &StatusSections) -> bool {
    !message.trim().is_empty() && !sections.staged.is_empty() && !sections.has_conflicts()
}

/// The commit button's label, carrying the live staged count.
pub fn commit_button_label(staged_count: usize) -> String {
    match staged_count {
        1 => "Commit 1 file".into(),
        n => format!("Commit {n} files"),
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
    /// The last refused stage/unstage's engine message (the error strip).
    /// Transient feedback, not persistent state: the next good frame
    /// clears it — the tree moved, so the refusal's premise is stale —
    /// while a refused write itself changes nothing and emits no frame,
    /// leaving the message up over exactly the state that refused it.
    write_error: Option<String>,
}

impl StatusView {
    fn apply(&mut self, frame: WorkspaceGitStatus) {
        self.frame_seen = true;
        let Some(workdir) = frame.workdir else {
            self.not_git = true;
            self.workdir = None;
            self.sections = StatusSections::default();
            self.error = None;
            self.write_error = None;
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
        // A good frame means the tree moved — any earlier refusal's premise
        // is stale, so the strip goes with it.
        self.write_error = None;
        self.sections = assign_sections(&frame.entries);
    }

    /// The stream itself dropped or never subscribed (engine restart): the
    /// retry loop re-arms, the banner rides the last good frame meanwhile.
    fn note_stream_error(&mut self, message: String) {
        self.error = Some(message);
    }

    /// A staging write the engine refused: its message rides the strip.
    fn note_write_error(&mut self, message: String) {
        self.write_error = Some(message);
    }

    /// A staging write landed: the strip goes, and the watch's next frame
    /// moves the rows — nothing here touches section state.
    fn clear_write_error(&mut self) {
        self.write_error = None;
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
    /// The commit message draft — the shared composer input entity, so the
    /// commit box behaves exactly like every other multiline field (Enter
    /// submits, Shift-Enter breaks the line, full IME/undo machinery).
    message: Entity<ComposerInput>,
    /// Holds the draft's event subscription for the panel's lifetime.
    _message_events: Subscription,
    /// One commit write is in flight: the button and Enter both refuse
    /// until its reply lands.
    committing: bool,
    /// The last commit's short sha, flashed briefly beside the button.
    committed_flash: Option<String>,
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
        let message = cx.new(|cx| ComposerInput::new("Commit message…", cx));
        let _message_events = cx.subscribe(&message, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_staged(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let mut panel = Self {
            state,
            chat_id,
            space_id,
            view: StatusView::default(),
            watch: None,
            diff_watch: None,
            diffs: Vec::new(),
            message,
            _message_events,
            committing: false,
            committed_flash: None,
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

    /// The address and transport every git write needs: the stream's own
    /// workdir — the checkout whose status the rows describe, so the write
    /// addresses exactly what is rendered — plus the engine handle. `None`
    /// when there is nothing to address yet (no workdir seen, or no engine
    /// attached).
    fn write_target(&self, cx: &Context<Self>) -> Option<(String, EngineHandle)> {
        let repo_path = self.view.workdir.clone()?;
        let engine = self.state.read(cx).engine().cloned()?;
        Some((repo_path, engine))
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

    /// Switch the internal tab. Becoming visible is one of History's
    /// exactly two refresh triggers: the entity is created (and loads) on
    /// first show, then force-reloads on every later visit. Branch
    /// switches made elsewhere while the panel stays open stay stale
    /// until either trigger fires, by design — the status payload
    /// deliberately carries no head sha to key a smarter invalidation.
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

    /// Issue one staging write through the engine's trio. No optimistic
    /// state: on success only the status watch's next frame moves the rows;
    /// on refusal the engine's message rides the error strip and the rows
    /// keep telling the truth. No Turn gating — the per-checkout lock
    /// serializes this with any agent git work, mid-Turn included
    /// (ADR-0022).
    fn run_staging(&mut self, action: StagingAction, cx: &mut Context<Self>) {
        if action.paths().is_empty() {
            return;
        }
        let Some((repo_path, engine)) = self.write_target(cx) else {
            return;
        };
        let (method, paths) = match action {
            StagingAction::Stage(paths) => (methods::STAGE_PATHS, paths),
            StagingAction::Unstage(paths) => (methods::UNSTAGE_PATHS, paths),
        };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    method,
                    serde_json::json!({ "repoPath": repo_path, "paths": paths }),
                )
                .await;
            this.update(cx, |panel, cx| {
                match result {
                    Ok(_) => panel.view.clear_write_error(),
                    Err(err) => panel.view.note_write_error(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Commit the staged index. The UI gate is presentation only (Enter and
    /// the button share it); the engine's own gates — missing identity,
    /// mid-merge state, an empty index — are the backstop and land their
    /// message on the error strip with the draft preserved for a retry. No
    /// Turn gating: committing mid-Turn rides the same per-checkout lock
    /// as staging (ADR-0022).
    fn commit_staged(&mut self, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let message = self.message.read(cx).text().trim().to_string();
        if !commit_enabled(&message, &self.view.sections) {
            return;
        }
        let Some((repo_path, engine)) = self.write_target(cx) else {
            return;
        };
        self.committing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::COMMIT_STAGED,
                    serde_json::json!({ "repoPath": repo_path, "message": message }),
                )
                .await;
            this.update(cx, |panel, cx| {
                panel.committing = false;
                match result {
                    Ok(value) => {
                        let sha = value
                            .get("sha")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        panel.view.clear_write_error();
                        panel.message.update(cx, |input, cx| input.set_text("", cx));
                        // The contract always replies with a sha; an empty
                        // one (a degenerate reply) still clears the draft,
                        // just without a flash.
                        if !sha.is_empty() {
                            panel.flash_commit(sha, cx);
                        }
                        // History's first refresh trigger (ticket 07): the
                        // returned sha is the new HEAD, so a forced page-0
                        // reload lands it as the graph's marked head row.
                        // A never-opened History tab needs nothing — its
                        // first show loads fresh.
                        if let Some(history) = panel.history.clone() {
                            history.update(cx, |history, cx| history.reload(cx));
                        }
                    }
                    Err(err) => panel.view.note_write_error(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Flash the new commit's short sha beside the button for a few
    /// seconds. A newer flash replaces an older one outright, and only the
    /// matching timer may clear what is showing.
    fn flash_commit(&mut self, sha: &str, cx: &mut Context<Self>) {
        let short = sha.get(..7).unwrap_or(sha).to_string();
        self.committed_flash = Some(short.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COMMIT_FLASH).await;
            this.update(cx, |panel, cx| {
                if panel.committed_flash.as_deref() == Some(short.as_str()) {
                    panel.committed_flash = None;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The click wiring the bulk toolbar buttons share: pointer
    /// affordance, a hover wash, and the write itself. (Row checkboxes
    /// stop propagation on top of this — their row opens the diff.)
    fn staging_clickable(
        el: gpui::Stateful<gpui::Div>,
        hover: impl FnOnce(gpui::StyleRefinement) -> gpui::StyleRefinement,
        action: StagingAction,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.cursor_pointer()
            .hover(hover)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.run_staging(action.clone(), cx);
            }))
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

    /// One status row: checkbox, kind badge, path. The checkbox is checked
    /// exactly where its click unstages — the Staged side — and unchecked
    /// where it stages; a conflicted row has no action and renders
    /// disabled. The checkbox is its own click target (stopping
    /// propagation so the row's diff-open click doesn't fire under it),
    /// while clicking anywhere else on the row opens the Changes surface
    /// scrolled to this file (ticket 06). The row's visible state only
    /// ever follows the stream, never a local guess; rows carry kind
    /// badges only — no per-file statistics.
    fn render_row(
        &self,
        theme: &Theme,
        ix: usize,
        row: &StatusRow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let checkbox_state = match row.staging_action() {
            None => CheckboxState::Disabled,
            Some(StagingAction::Unstage(_)) => CheckboxState::Checked,
            Some(StagingAction::Stage(_)) => CheckboxState::Unchecked,
        };
        // The staging click target: a padded cell around the 14px box, so
        // the hit area is honest without inflating the visual weight.
        let mut checkbox = div()
            .id(SharedString::from(format!(
                "git-status-check-{}-{ix}",
                row.section.id_tag()
            )))
            .flex_none()
            .size(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .child(widgets::checkbox(theme, checkbox_state));
        if let Some(action) = row.staging_action() {
            checkbox = checkbox
                .cursor_pointer()
                .hover(|state| state.bg(crate::theme::wash(0.06)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    // The row's own click (open the diff) must not fire.
                    cx.stop_propagation();
                    this.run_staging(action.clone(), cx);
                }));
        }
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
            .child(checkbox)
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
            // Inline hints: conflicted rows explain their disabled checkbox;
            // untracked directories disclose the staging scope.
            .when(row.conflicted, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.danger.opacity(0.9))
                        .child("conflict"),
                )
            })
            .when(row.is_dir, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child("stages the whole directory"),
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

    /// The fixed Stage all / Unstage all bar under the header. Buttons
    /// disable when their side has nothing to move (conflicted rows never
    /// count); the bar hides entirely while loading, on a non-git root,
    /// and on a clean tree.
    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let stage_paths = self.view.sections.stage_all_paths();
        let unstage_paths = self.view.sections.unstage_all_paths();
        div()
            .flex_none()
            .h(px(30.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .child(Self::toolbar_button(
                theme,
                "git-stage-all",
                "Stage all",
                !stage_paths.is_empty(),
                StagingAction::Stage(stage_paths),
                cx,
            ))
            .child(Self::toolbar_button(
                theme,
                "git-unstage-all",
                "Unstage all",
                !unstage_paths.is_empty(),
                StagingAction::Unstage(unstage_paths),
                cx,
            ))
            .into_any_element()
    }

    /// One quiet toolbar action; disabled carries no click and dims.
    fn toolbar_button(
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        enabled: bool,
        action: StagingAction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut button = div()
            .id(id)
            .flex_none()
            .h(px(20.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(px(5.0))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(if enabled {
                theme.text_muted
            } else {
                theme.text_muted.opacity(0.45)
            });
        if enabled {
            button = Self::staging_clickable(
                button,
                |state| state.bg(crate::theme::wash(0.06)).text_color(theme.text),
                action,
                cx,
            );
        }
        button.child(label).into_any_element()
    }

    /// The commit box pinned to the panel's bottom (ticket 05): the shared
    /// composer input for the message — Enter commits, Shift-Enter breaks
    /// the line, exactly the main composer's chords — above a footer row
    /// pairing the flashed short sha with the count-labeled Commit button.
    /// The button dims and refuses while the gate fails or a write is in
    /// flight; a failed commit keeps the draft for a retry.
    fn render_commit_box(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let enabled =
            commit_enabled(self.message.read(cx).text(), &self.view.sections) && !self.committing;
        let staged_count = self.view.sections.staged.len();
        let mut button = div()
            .id("git-commit-button")
            .flex_none()
            .h(px(24.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .bg(theme.solid)
            .text_color(theme.on_solid)
            .child(SharedString::from(commit_button_label(staged_count)));
        if enabled {
            button = button
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.commit_staged(cx);
                }));
        } else {
            button = button.opacity(0.35);
        }
        div()
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .px(px(10.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .w_full()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.hairline(0.12))
                    .bg(theme.ink(0.04))
                    .px(px(10.0))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .child(self.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0())
                    .children(self.committed_flash.clone().map(|sha| {
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                icon(icons::CHECK)
                                    .size(px(12.0))
                                    .flex_none()
                                    .text_color(theme.success),
                            )
                            .child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.success_muted)
                                    .child(SharedString::from(format!("Committed {sha}"))),
                            )
                    }))
                    .child(button),
            )
            .into_any_element()
    }

    /// The staging-refusal strip (the file viewer's save-error pattern):
    /// the engine's message verbatim, over rows that still tell the truth.
    fn write_error_banner(theme: &Theme, message: SharedString) -> AnyElement {
        div()
            .flex_none()
            .px(px(12.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.danger.opacity(0.08))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                icon(icons::DANGER_TRIANGLE)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.danger_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    // Wraps, never truncates: the engine's refusal is the
                    // actionable message (missing identity, unresolved
                    // conflict) and must arrive whole.
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.danger_muted)
                    .child(message),
            )
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
            // A refused stage/unstage strip carries the engine's message
            // verbatim over rows that still tell the truth about the index —
            // Status-tab chrome.
            .when_some(
                self.view.write_error.clone().filter(|_| on_status),
                |el, message| el.child(Self::write_error_banner(&theme, message.into())),
            )
            // The bulk actions ride a fixed bar under the header — hidden
            // while loading, on a non-git root, and on a clean tree.
            .when(
                on_status
                    && self.view.frame_seen
                    && !self.view.not_git
                    && !self.view.sections.is_empty(),
                |el| el.child(self.render_toolbar(&theme, cx)),
            )
            // A read failure / stream interruption banners over the last
            // good frame (the Changes pane's watch-error pattern); a non-git
            // root banners with its controls absent (no rows render). Both
            // tabs — they describe the checkout, not the status list.
            .when_some(self.view.error.clone(), |el, message| {
                el.child(Self::banner(&theme, message))
            })
            .when(self.view.not_git, |el| {
                el.child(Self::banner(
                    &theme,
                    "Not a git repository — status controls are off",
                ))
            })
            .child(div().flex_1().min_h_0().child(body))
            // The commit box rides pinned to the bottom of the Status tab —
            // clean tree included, with the button gated off. History is a
            // read view: no composer.
            .when(
                on_status && self.view.frame_seen && !self.view.not_git,
                |el| el.child(self.render_commit_box(&theme, cx)),
            )
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

    #[test]
    fn a_rows_checkbox_acts_on_its_own_half() {
        let sections = assign_sections(&[
            entry(
                "src/lib.rs",
                Kind::Modified,
                Some(Kind::Modified),
                Some(Kind::Modified),
                false,
            ),
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
        // The staged half's uncheck unstages…
        assert_eq!(
            sections.staged[0].staging_action(),
            Some(StagingAction::Unstage(vec!["src/lib.rs".into()]))
        );
        // …while the same file's unstaged half checks to stage — each row
        // acts on its own side.
        assert_eq!(
            sections.unstaged[0].staging_action(),
            Some(StagingAction::Stage(vec!["src/lib.rs".into()]))
        );
        // Untracked files and whole directories stage (the directory
        // recursively, engine-side).
        assert_eq!(
            sections.untracked[0].staging_action(),
            Some(StagingAction::Stage(vec!["notes.md".into()]))
        );
        assert_eq!(
            sections.untracked[1].staging_action(),
            Some(StagingAction::Stage(vec!["prototype".into()]))
        );
    }

    #[test]
    fn conflicted_rows_offer_no_action_in_either_section() {
        let sections = assign_sections(&[entry(
            "src/merge.rs",
            Kind::Conflicted,
            Some(Kind::Conflicted),
            Some(Kind::Conflicted),
            false,
        )]);
        assert_eq!(sections.staged[0].staging_action(), None);
        assert_eq!(sections.unstaged[0].staging_action(), None);
    }

    #[test]
    fn stage_all_covers_unstaged_and_untracked_but_never_conflicted() {
        let sections = assign_sections(&[
            entry("a.rs", Kind::Modified, None, Some(Kind::Modified), false),
            entry("b.rs", Kind::Untracked, None, Some(Kind::Untracked), false),
            entry("pkg", Kind::Untracked, None, Some(Kind::Untracked), true),
            entry("c.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry(
                "conf.rs",
                Kind::Conflicted,
                None,
                Some(Kind::Conflicted),
                false,
            ),
        ]);
        let mut paths = sections.stage_all_paths();
        paths.sort();
        assert_eq!(paths, ["a.rs", "b.rs", "pkg"]);
    }

    #[test]
    fn unstage_all_covers_staged_but_never_conflicted() {
        let sections = assign_sections(&[
            entry("a.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry("b.rs", Kind::Added, Some(Kind::Added), None, false),
            entry(
                "conf.rs",
                Kind::Conflicted,
                Some(Kind::Conflicted),
                Some(Kind::Conflicted),
                false,
            ),
        ]);
        let mut paths = sections.unstage_all_paths();
        paths.sort();
        assert_eq!(paths, ["a.rs", "b.rs"]);
    }

    #[test]
    fn a_refused_write_errors_the_strip_and_never_touches_the_rows() {
        let mut view = StatusView::default();
        view.apply(frame(
            Some("/repo"),
            vec![entry(
                "a.rs",
                Kind::Modified,
                None,
                Some(Kind::Modified),
                false,
            )],
            None,
        ));
        view.note_write_error("a.rs has unresolved merge conflicts — resolve them first".into());
        assert_eq!(
            view.write_error.as_deref(),
            Some("a.rs has unresolved merge conflicts — resolve them first")
        );
        assert_eq!(view.sections.unstaged.len(), 1);

        // A read-failure frame is not a tree move: the strip stays.
        view.apply(frame(Some("/repo"), Vec::new(), Some("corrupt index")));
        assert!(view.write_error.is_some());
        assert_eq!(
            view.sections.unstaged.len(),
            1,
            "the error frame never clears the last good rows"
        );

        // The next good frame means the tree moved — the external-terminal
        // case, e.g. the conflict resolved out there — so the refusal's
        // premise is stale and the strip goes.
        view.apply(frame(Some("/repo"), Vec::new(), None));
        assert_eq!(view.write_error, None);
        assert!(view.sections.is_empty());
    }

    #[test]
    fn the_commit_button_needs_message_and_staged_paths() {
        let sections = assign_sections(&[
            entry("a.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry("b.rs", Kind::Added, Some(Kind::Added), None, false),
        ]);
        assert!(
            commit_enabled("Fix the thing", &sections),
            "message plus staged paths enables"
        );
        assert!(!commit_enabled("", &sections), "a blank message disables");
        assert!(
            !commit_enabled("   \n\t ", &sections),
            "a whitespace-only message is blank"
        );
    }

    #[test]
    fn the_commit_button_needs_something_staged() {
        let clean = StatusSections::default();
        assert!(!commit_enabled("Fix the thing", &clean));

        let unstaged_only = assign_sections(&[entry(
            "a.rs",
            Kind::Modified,
            None,
            Some(Kind::Modified),
            false,
        )]);
        assert!(
            !commit_enabled("Fix the thing", &unstaged_only),
            "worktree-side changes alone are not committable"
        );

        let untracked_only = assign_sections(&[entry(
            "new.rs",
            Kind::Untracked,
            None,
            Some(Kind::Untracked),
            false,
        )]);
        assert!(!commit_enabled("Fix the thing", &untracked_only));
    }

    #[test]
    fn any_conflict_disables_the_commit_button() {
        // A conflict in the Staged section alone…
        let staged_conflict = assign_sections(&[entry(
            "conf.rs",
            Kind::Conflicted,
            Some(Kind::Conflicted),
            Some(Kind::Conflicted),
            false,
        )]);
        assert!(!commit_enabled("Merge work", &staged_conflict));

        // …and one sitting beside otherwise-clean staged paths.
        let mixed = assign_sections(&[
            entry("a.rs", Kind::Modified, Some(Kind::Modified), None, false),
            entry(
                "conf.rs",
                Kind::Conflicted,
                None,
                Some(Kind::Conflicted),
                false,
            ),
        ]);
        assert!(
            !commit_enabled("Merge work", &mixed),
            "a conflicted tree is mid-merge — the engine would refuse"
        );
    }

    #[test]
    fn the_commit_button_label_carries_the_staged_count() {
        assert_eq!(commit_button_label(0), "Commit 0 files");
        assert_eq!(commit_button_label(1), "Commit 1 file");
        assert_eq!(commit_button_label(3), "Commit 3 files");
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
            approved_plan_path: None,
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
        let (panel, mut visual) = cx.add_window_view(|_window, cx| GitPanel::new(state, cx));
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
