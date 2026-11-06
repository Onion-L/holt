//! The integration page (glossary: integration view): one repository's
//! linked worktrees, listed like a pull-request inbox. Opening a row
//! keeps the inbox on the left and shows that worktree's summary on the
//! right. Data comes from `ListIntegration` plus the chats/sessions
//! watches the shell already holds.

pub(crate) mod detail;
mod graph;

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Render, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use holt_proto::{IntegrationSnapshot, IntegrationWorktree, SessionStatus, Space};
use holt_rpc::methods;

use crate::changes::Changes;
use crate::composer::ComposerInput;
use crate::i18n::t;
use crate::icons::{self, icon};
use crate::popover;
use crate::state::AppState;
use crate::theme::Theme;

use detail::Detail;
use graph::{RowKind, TreeLayout};

/// The inbox column. Wider than a chat transcript, narrow enough that a
/// row still reads as one line.
const INBOX_MAX_W: f32 = 720.0;

pub enum IntegrationEvent {
    OpenChat(String),
    /// Open the worktree's chat and its terminal (the chat's working
    /// directory is the worktree). `path` rides along for future chatless
    /// worktrees.
    OpenTerminal {
        chat_id: String,
        path: String,
    },
    /// Open a path in the user's chosen external application.
    OpenExternally(String),
    /// A toast-style notice; `success` picks the check icon.
    Notice {
        message: String,
        success: bool,
    },
}

/// The merge mode the bar offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeMode {
    Squash,
    FastForward,
}

impl MergeMode {
    fn wire(self) -> &'static str {
        match self {
            MergeMode::Squash => "squash",
            MergeMode::FastForward => "ff",
        }
    }

    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            MergeMode::Squash => t!("integration.merge.mode_squash"),
            MergeMode::FastForward => t!("integration.merge.mode_ff"),
        }
    }
}

/// One worktree's merge, from the preview click to the landed commit (or
/// the refusal that stopped it).
struct MergeFlow {
    worktree: String,
    state: MergeState,
}

#[derive(Clone)]
enum MergeState {
    Previewing,
    /// The confirm dialog: the preview, the editable message, the diff fold.
    Dialog {
        preview: holt_proto::MergePreview,
        message: Entity<ComposerInput>,
        diff_open: bool,
    },
    Merging,
    Conflict {
        paths: Vec<String>,
    },
    Refused {
        reason: holt_proto::MergeRefusal,
        detail: Vec<String>,
    },
    Failed {
        message: SharedString,
    },
}

/// The "hand the conflicts to the author chat" dialog.
struct ResolveDialog {
    worktree: String,
    input: Entity<ComposerInput>,
}

/// The review card's dialogs: a one-off custom prompt, or the editable
/// send-back into the author chat.
enum ReviewDialog {
    Custom {
        input: Entity<ComposerInput>,
    },
    Send {
        chat_id: String,
        input: Entity<ComposerInput>,
    },
}

/// The worktree-delete confirmation. `dirty` paths take the force flag and
/// the data-loss warning.
struct DeleteConfirm {
    worktree: String,
    dirty: bool,
}

/// The worktree status chip — derived per render (never stored): the first
/// matching rule of the design's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeStatus {
    Missing,
    Working,
    Merged,
    Dirty,
    NoChanges,
    Behind,
    Ready,
}

pub(crate) fn worktree_status(
    worktree: &IntegrationWorktree,
    session_working: bool,
) -> WorktreeStatus {
    if worktree.missing {
        WorktreeStatus::Missing
    } else if worktree.chat_id.is_some() && session_working {
        WorktreeStatus::Working
    } else if worktree.merged {
        WorktreeStatus::Merged
    } else if worktree.dirty {
        WorktreeStatus::Dirty
    } else if worktree.ahead == 0 {
        WorktreeStatus::NoChanges
    } else if worktree.behind > 0 {
        WorktreeStatus::Behind
    } else {
        WorktreeStatus::Ready
    }
}

/// A worktree under the engine's worktrees directory whose owning chat is
/// gone — its data is stranded, not project work.
pub(crate) fn worktree_is_orphan(worktree: &IntegrationWorktree, data_dir: &Path) -> bool {
    worktree.chat_id.is_none() && {
        let root = data_dir.join("worktrees");
        Path::new(&worktree.path).starts_with(&root)
    }
}

/// The list filter. Non-matching worktrees hide — the list is the
/// worktrees, not a commit graph that a gap would tear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Active,
    Ready,
    Blocked,
    Merged,
}

fn filter_label(filter: Filter) -> std::borrow::Cow<'static, str> {
    match filter {
        Filter::All => t!("integration.filter.all"),
        Filter::Active => t!("integration.filter.active"),
        Filter::Ready => t!("integration.filter.ready"),
        Filter::Blocked => t!("integration.filter.blocked"),
        Filter::Merged => t!("integration.filter.merged"),
    }
}

pub(crate) fn filter_matches(filter: Filter, status: WorktreeStatus) -> bool {
    match filter {
        Filter::All => true,
        Filter::Active => matches!(
            status,
            WorktreeStatus::Working | WorktreeStatus::Dirty | WorktreeStatus::NoChanges
        ),
        Filter::Ready => matches!(status, WorktreeStatus::Ready | WorktreeStatus::Behind),
        // No conflict detection until the merge slice; the attention state
        // P0 has is a missing directory.
        Filter::Blocked => status == WorktreeStatus::Missing,
        Filter::Merged => status == WorktreeStatus::Merged,
    }
}

/// One offered repository: a project space's path. Home and session-worktree
/// spaces never appear; duplicate paths collapse to the first row.
pub(crate) struct RepoOption {
    pub label: SharedString,
    pub path: String,
}

pub(crate) fn repo_options(spaces: &[Space]) -> Vec<RepoOption> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut options: Vec<(String, RepoOption)> = spaces
        .iter()
        .filter(|space| {
            space.git_detected
                && space.id != holt_proto::HOME_SPACE_ID
                && !space.id.starts_with("wt-")
        })
        .filter(|space| seen.insert(space.path.trim_end_matches('/').to_string()))
        .map(|space| {
            (
                space.display_name().to_lowercase(),
                RepoOption {
                    label: SharedString::from(space.display_name().to_string()),
                    path: space.path.trim_end_matches('/').to_string(),
                },
            )
        })
        .collect();
    options.sort_by(|a, b| a.0.cmp(&b.0));
    options.into_iter().map(|(_, option)| option).collect()
}

/// Which repo the page opens on: the selected chat's session-worktree's
/// repo, else the selected chat's space, else the selected space, else the
/// first option.
fn default_repo(state: &AppState, options: &[RepoOption]) -> Option<String> {
    let contains = |path: &str| options.iter().any(|option| option.path == path);
    if let Some(chat) = state.selected_chat_row() {
        if let Some(spec) = &chat.worktree
            && contains(&spec.repo_path)
        {
            return Some(spec.repo_path.clone());
        }
        if let Some(space) = chat.space_id.as_deref().and_then(|id| state.space_row(id))
            && contains(space.path.trim_end_matches('/'))
        {
            return Some(space.path.trim_end_matches('/').to_string());
        }
    }
    if let Some(space) = state
        .selected_space
        .as_deref()
        .and_then(|id| state.space_row(id))
        && contains(space.path.trim_end_matches('/'))
    {
        return Some(space.path.trim_end_matches('/').to_string());
    }
    options.first().map(|option| option.path.clone())
}

pub struct IntegrationPage {
    state: Entity<AppState>,
    /// The first render picked the repo and fetched once.
    started: bool,
    pub(crate) repo_path: Option<String>,
    /// The user's explicit trunk (never persisted); `None` = the repo's
    /// default.
    trunk: Option<String>,
    refs: Vec<String>,
    refs_for: Option<String>,
    pub(crate) snapshot: Option<IntegrationSnapshot>,
    pub(crate) layout: Option<TreeLayout>,
    loading: bool,
    pub(crate) error: Option<SharedString>,
    fetching: bool,
    filter: Filter,
    selected: Option<String>,
    detail: Option<Detail>,
    repo_menu: popover::Popup<()>,
    trunk_menu: popover::Popup<()>,
    merge_mode: MergeMode,
    merge: Option<MergeFlow>,
    checks_watch: Option<Task<()>>,
    checks_run: Option<holt_rpc::checks::CheckRun>,
    reviewers: Vec<(String, String)>,
    reviews: Vec<serde_json::Value>,
    reviews_watch: Option<Task<()>>,
    /// The first `ListReviews` for the selection landed (so "not reviewed"
    /// is a fact, not a loading state).
    reviews_loaded: bool,
    /// Finding ids picked for a multi-send.
    selected_findings: HashSet<String>,
    review_dialog: Option<ReviewDialog>,
    /// The composer's pickers: a review of a chatless worktree takes their
    /// resolved model.
    pickers: Entity<crate::pickers::Pickers>,
    checks_error: Option<SharedString>,
    checks_log: Option<(String, Entity<CheckLogView>)>,
    resolve: Option<ResolveDialog>,
    delete_confirm: Option<DeleteConfirm>,
    deleting: Option<String>,
    focus: FocusHandle,
    focus_pending: bool,
    task: Option<Task<()>>,
    refs_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    _observe: Subscription,
}

impl IntegrationPage {
    pub fn new(
        state: Entity<AppState>,
        pickers: Entity<crate::pickers::Pickers>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |this, _, cx| this.on_state_changed(cx));
        Self {
            state,
            started: false,
            repo_path: None,
            trunk: None,
            refs: Vec::new(),
            refs_for: None,
            snapshot: None,
            layout: None,
            loading: false,
            error: None,
            fetching: false,
            filter: Filter::All,
            selected: None,
            detail: None,
            repo_menu: popover::Popup::default(),
            trunk_menu: popover::Popup::default(),
            merge_mode: MergeMode::Squash,
            merge: None,
            checks_watch: None,
            checks_run: None,
            reviewers: Vec::new(),
            reviews: Vec::new(),
            reviews_watch: None,
            reviews_loaded: false,
            selected_findings: HashSet::new(),
            review_dialog: None,
            pickers,
            checks_error: None,
            checks_log: None,
            resolve: None,
            delete_confirm: None,
            deleting: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            task: None,
            refs_task: None,
            fetch_task: None,
            _observe: observe,
        }
    }

    /// The shell shows the page again — re-land keyboard focus and pull a
    /// fresh snapshot (each open fetches once; the refresh button is the
    /// other trigger).
    pub(crate) fn reveal(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        if self.repo_path.is_some() && !self.loading && self.fetch_task.is_none() {
            self.load(cx);
        }
        cx.notify();
    }

    /// The lazy pane's first-fetch hook (called from render, guarded by
    /// `started` — the pane is created mid-render with no notify of its own
    /// to ride, so without this the page would sit empty until some
    /// unrelated state change arrived).
    fn ensure_started(&mut self, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        if let Some(default) = {
            let state = self.state.read(cx);
            let options = repo_options(&state.spaces);
            default_repo(state, &options)
        } {
            self.started = true;
            self.repo_path = Some(default);
            self.load(cx);
            self.fetch_refs(cx);
        }
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (options, default) = {
            let state = self.state.read(cx);
            let options = repo_options(&state.spaces);
            let default = default_repo(state, &options);
            (options, default)
        };
        // The selected repo's space vanished: drop the view state.
        if let Some(current) = &self.repo_path
            && !options.iter().any(|option| &option.path == current)
        {
            self.repo_path = None;
            self.trunk = None;
            self.snapshot = None;
            self.layout = None;
            self.refs.clear();
            self.refs_for = None;
            self.selected = None;
            self.detail = None;
        }
        if self.repo_path.is_none()
            && let Some(default) = default
        {
            self.repo_path = Some(default);
            self.load(cx);
            self.fetch_refs(cx);
        }
        // Reviews land on chat rows — a chats change re-reads the list.
        if self.reviews_watch.is_some() {
            self.reviews_watch = None;
        }
        // The session-status re-derivation needs no fetch; the render reads
        // the sessions watch live.
        cx.notify();
    }

    fn close_repo_menu(&mut self, cx: &mut Context<Self>) {
        if self.repo_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.repo_menu);
            cx.notify();
        }
    }

    fn close_trunk_menu(&mut self, cx: &mut Context<Self>) {
        if self.trunk_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.trunk_menu);
            cx.notify();
        }
    }

    /// Select `path`'s snapshot: reset trunk and view state, reload.
    fn select_repo(&mut self, path: String, cx: &mut Context<Self>) {
        if self.repo_path.as_deref() == Some(path.as_str()) {
            return;
        }
        self.repo_path = Some(path);
        self.trunk = None;
        self.snapshot = None;
        self.layout = None;
        self.refs.clear();
        self.refs_for = None;
        self.selected = None;
        self.detail = None;
        self.error = None;
        self.load(cx);
        self.fetch_refs(cx);
        cx.notify();
    }

    fn select_trunk(&mut self, trunk: String, cx: &mut Context<Self>) {
        self.trunk = Some(trunk);
        self.load(cx);
        cx.notify();
    }

    fn select_worktree(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name) {
            self.show_list(cx);
            return;
        }
        let trunk = self.trunk_of().unwrap_or_default();
        let reuse = self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.worktree == name && detail.trunk == trunk);
        self.selected = Some(name.to_string());
        if reuse {
            cx.notify();
            return;
        }
        self.detail = Detail::rebuild(self.detail.take(), name, trunk);
        self.merge = None;
        self.resolve = None;
        self.delete_confirm = None;
        self.checks_watch = None;
        self.checks_run = None;
        self.checks_error = None;
        self.checks_log = None;
        self.reviews_watch = None;
        self.reviewers.clear();
        self.reviews.clear();
        self.reviews_loaded = false;
        self.selected_findings.clear();
        self.review_dialog = None;
        cx.notify();
    }

    /// Leave the open worktree and return to the inbox. The detail stays
    /// built so opening the same row again does not refetch.
    fn show_list(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        cx.notify();
    }

    /// The trunk this page currently shows.
    fn trunk_of(&self) -> Option<String> {
        self.trunk
            .clone()
            .or_else(|| self.snapshot.as_ref().map(|s| s.trunk.name.clone()))
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let trunk = self.trunk.clone();
        self.loading = true;
        self.error = None;
        cx.notify();
        self.task =
            Some(cx.spawn(async move |this, cx| {
                let mut params = serde_json::Map::new();
                params.insert(
                    "repoPath".into(),
                    serde_json::Value::String(repo_path.clone()),
                );
                if let Some(trunk) = trunk {
                    params.insert("trunk".into(), serde_json::Value::String(trunk));
                }
                let result = engine
                    .client()
                    .call(methods::LIST_INTEGRATION, serde_json::Value::Object(params))
                    .await;
                this.update(cx, |page, cx| {
                    page.loading = false;
                    match result.and_then(|value| {
                        serde_json::from_value::<IntegrationSnapshot>(value)
                            .map_err(|error| holt_rpc::RpcError::Failed(error.to_string()))
                    }) {
                        Ok(snapshot) => {
                            page.error = None;
                            page.layout = Some(graph::layout(&snapshot));
                            // The list is the page. A reload keeps an open
                            // worktree; it does not open the first one.
                            if page.selected.as_ref().is_some_and(|name| {
                                !snapshot.worktrees.iter().any(|w| &w.name == name)
                            }) {
                                page.selected = None;
                                page.detail = None;
                            }
                            if let Some(selected) = page.selected.clone() {
                                let trunk_now = page
                                    .trunk
                                    .clone()
                                    .unwrap_or_else(|| snapshot.trunk.name.clone());
                                let key_matches = page.detail.as_ref().is_some_and(|detail| {
                                    detail.worktree == selected && detail.trunk == trunk_now
                                });
                                if !key_matches {
                                    page.detail =
                                        Detail::rebuild(page.detail.take(), &selected, trunk_now);
                                } else if let Some(detail) = &page.detail {
                                    detail.invalidate(cx);
                                }
                            }
                            page.snapshot = Some(snapshot);
                        }
                        Err(error) => {
                            page.error = Some(error.to_string().into());
                            page.snapshot = None;
                            page.layout = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
    }

    /// The trunk picker's branch list.
    fn fetch_refs(&mut self, cx: &mut Context<Self>) {
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        if self.refs_for.as_deref() == Some(repo_path.as_str()) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.refs_for = Some(repo_path.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(repo_path.clone()),
            );
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |page, cx| {
                if page.refs_for.as_deref() != Some(repo_path.as_str()) {
                    return;
                }
                if let Ok(value) = result {
                    page.refs = serde_json::from_value::<Vec<holt_proto::RepoRef>>(value)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| r.name)
                        .collect();
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The header's refresh: `FetchAll` first, then a fresh snapshot.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.fetching {
            return;
        }
        self.fetching = true;
        cx.notify();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(repo_path.clone()),
            );
            let fetched = engine
                .client()
                .call(methods::FETCH_ALL, serde_json::Value::Object(params))
                .await;
            this.update(cx, |page, cx| {
                page.fetching = false;
                if let Err(error) = fetched {
                    // A fetch failure (offline, no remote) must not hide the
                    // snapshot — the refresh still reloads local state.
                    tracing::debug!(error = %error, "integration: fetch failed");
                }
                page.load(cx);
                page.fetch_refs(cx);
            })
            .ok();
        }));
    }

    /// The worktree rows' statuses, keyed by name (derived per render).
    fn statuses(&self, cx: &App) -> std::collections::HashMap<String, (WorktreeStatus, bool)> {
        let Some(snapshot) = &self.snapshot else {
            return std::collections::HashMap::new();
        };
        let state = self.state.read(cx);
        let data_dir = state.data_dir.clone().unwrap_or_default();
        snapshot
            .worktrees
            .iter()
            .map(|worktree| {
                let working = worktree
                    .chat_id
                    .as_deref()
                    .and_then(|id| state.session_for(id))
                    .is_some_and(|session| {
                        matches!(
                            session.status,
                            SessionStatus::Working | SessionStatus::Compacting
                        )
                    });
                (
                    worktree.name.clone(),
                    (
                        worktree_status(worktree, working),
                        worktree_is_orphan(worktree, &data_dir),
                    ),
                )
            })
            .collect()
    }

    fn chat_title(&self, chat_id: &str, cx: &App) -> Option<SharedString> {
        let chat = self.state.read(cx).chat_row(chat_id)?;
        let title = chat
            .title
            .clone()
            .unwrap_or_else(|| t!("integration.detail.untitled_chat").into_owned());
        Some(SharedString::from(crate::transcript::single_line(&title)))
    }

    // ---- render pieces ----

    pub(crate) fn status_chip(status: WorktreeStatus, trunk: &str, theme: &Theme) -> AnyElement {
        let (label, color): (Cow<'static, str>, _) = match status {
            WorktreeStatus::Missing => (t!("integration.status.missing"), theme.danger),
            WorktreeStatus::Working => (t!("integration.status.working"), theme.busy),
            WorktreeStatus::Merged => (t!("integration.status.merged"), theme.text_faint),
            WorktreeStatus::Dirty => (t!("integration.status.dirty"), theme.warning),
            WorktreeStatus::NoChanges => (t!("integration.status.no_changes"), theme.text_faint),
            WorktreeStatus::Behind => (
                t!("integration.status.behind", trunk = trunk),
                theme.text_muted,
            ),
            WorktreeStatus::Ready => (t!("integration.status.ready"), theme.success),
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(div().size(px(6.0)).rounded_full().bg(color))
            .child(SharedString::from(label.into_owned()))
            .into_any_element()
    }

    /// The summary card's status pill.
    pub(crate) fn status_badge(status: WorktreeStatus, trunk: &str, theme: &Theme) -> AnyElement {
        let (label, color): (Cow<'static, str>, _) = match status {
            WorktreeStatus::Missing => (t!("integration.status.missing"), theme.danger),
            WorktreeStatus::Working => (t!("integration.status.working"), theme.busy),
            WorktreeStatus::Merged => (t!("integration.status.merged"), theme.text_faint),
            WorktreeStatus::Dirty => (t!("integration.status.dirty"), theme.warning),
            WorktreeStatus::NoChanges => (t!("integration.status.no_changes"), theme.text_faint),
            WorktreeStatus::Behind => (
                t!("integration.status.behind", trunk = trunk),
                theme.text_muted,
            ),
            WorktreeStatus::Ready => (t!("integration.status.ready"), theme.success),
        };
        div()
            .flex_none()
            .h(px(22.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(color.opacity(0.16))
            .text_size(px(12.0))
            .text_color(color)
            .child(SharedString::from(label.into_owned()))
            .into_any_element()
    }

    fn render_header(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let options = repo_options(&self.state.read(cx).spaces);
        let repo_label = self
            .repo_path
            .as_ref()
            .and_then(|path| options.iter().find(|option| &option.path == path))
            .map(|option| option.label.clone())
            .or_else(|| {
                // The space row vanished but the repo is still selected —
                // show the path's basename rather than nothing.
                self.repo_path.as_ref().map(|path| {
                    SharedString::from(
                        Path::new(path)
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.clone()),
                    )
                })
            })
            .unwrap_or_else(|| SharedString::from(t!("integration.repo_none")));
        let trunk = self.trunk_of().unwrap_or_default();
        let head_sha = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.trunk.head_sha.clone());
        let count = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.worktrees.len());
        let title = t!("integration.title");
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .child(crate::settings::widgets::page_header(
                theme,
                title.as_ref(),
                count,
            ))
            .child(div().flex_1())
            .child(self.render_refresh(theme, cx))
            .child(self.render_repo_trigger(repo_label, theme, cx))
            .when(!trunk.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(t!("integration.merge_into"))
                        .child(self.render_trunk_trigger(trunk.clone(), theme, cx))
                        .when_some(head_sha, |el, sha| {
                            el.child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.0))
                                    .text_color(theme.text_dim)
                                    .child(short_sha(&sha)),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn render_repo_trigger(
        &mut self,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let trigger = div()
            .id("integration-repo-trigger")
            .debug_selector(|| "integration-repo-trigger".into())
            .h(px(26.0))
            .px(px(9.0))
            .flex_none()
            .max_w(px(220.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.08)))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.repo_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                // The card's mouse-down-out already began the close. Reopening
                // here is what made the trigger look stuck shut.
                if this.repo_menu.take_press_was_open() {
                    this.close_repo_menu(cx);
                } else {
                    this.close_trunk_menu(cx);
                    this.repo_menu.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(label),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        if self.repo_menu.get().is_some() {
            let closing = self.repo_menu.closing_since();
            let menu = self.render_repo_menu(theme, cx);
            trigger.relative().child(popover::anchored_menu_below(
                "integration-repo-menu",
                menu,
                closing,
            ))
        } else {
            trigger
        }
    }

    fn render_repo_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let options = repo_options(&self.state.read(cx).spaces);
        let current = self.repo_path.clone();
        let rows: Vec<AnyElement> = options
            .iter()
            .map(|option| {
                let path = option.path.clone();
                let selected = current.as_deref() == Some(option.path.as_str());
                div()
                    .id(SharedString::from(format!("integration-repo-row-{path}")))
                    .min_w(px(220.0))
                    .max_w(px(340.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.element_hover))
                    .on_click({
                        let path = path.clone();
                        cx.listener(move |this, _, _, cx| {
                            this.close_repo_menu(cx);
                            this.select_repo(path.clone(), cx);
                        })
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(option.label.clone()),
                    )
                    .when(selected, |el| {
                        el.child(icon(icons::CHECK).size(px(13.0)).text_color(theme.accent))
                    })
                    .into_any_element()
            })
            .collect();
        popover::popover_card(theme)
            .w(px(300.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_repo_menu(cx)))
            .children(rows)
            .into_any_element()
    }

    fn render_trunk_trigger(
        &mut self,
        trunk: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let trigger = div()
            .id("integration-trunk-trigger")
            .h(px(24.0))
            .px(px(7.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.1)))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.trunk_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.trunk_menu.take_press_was_open() {
                    this.close_trunk_menu(cx);
                } else {
                    this.close_repo_menu(cx);
                    this.trunk_menu.open(());
                    cx.notify();
                }
            }))
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(trunk),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        if self.trunk_menu.get().is_some() {
            let closing = self.trunk_menu.closing_since();
            let menu = self.render_trunk_menu(theme, cx);
            trigger.relative().child(popover::anchored_menu_below(
                "integration-trunk-menu",
                menu,
                closing,
            ))
        } else {
            trigger
        }
    }

    fn render_trunk_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.trunk_of();
        let rows: Vec<AnyElement> = self
            .refs
            .iter()
            .map(|name| {
                let selected = current.as_deref() == Some(name.as_str());
                div()
                    .id(SharedString::from(format!("integration-trunk-row-{name}")))
                    .min_w(px(180.0))
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.element_hover))
                    .on_click({
                        let name = name.clone();
                        cx.listener(move |this, _, _, cx| {
                            this.close_trunk_menu(cx);
                            this.select_trunk(name.clone(), cx);
                        })
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(name.clone()),
                    )
                    .when(selected, |el| {
                        el.child(icon(icons::CHECK).size(px(13.0)).text_color(theme.accent))
                    })
                    .into_any_element()
            })
            .collect();
        popover::popover_card(theme)
            .id("integration-trunk-menu-list")
            .w(px(240.0))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .p(px(4.0))
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_trunk_menu(cx)))
            .children(rows)
            .into_any_element()
    }

    fn render_filters(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use Filter::{Active, All, Blocked, Merged, Ready};
        let filter = self.filter;
        let entries: [(Filter, SharedString); 5] = [
            (All, t!("integration.filter.all").into()),
            (Active, t!("integration.filter.active").into()),
            (Ready, t!("integration.filter.ready").into()),
            (Blocked, t!("integration.filter.blocked").into()),
            (Merged, t!("integration.filter.merged").into()),
        ];
        div()
            .id("integration-filters")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(8.0))
            .bg(crate::theme::wash(0.06))
            .children(entries.into_iter().map(|(entry, label)| {
                let active = entry == filter;
                div()
                    .id(SharedString::from(format!("integration-filter-{entry:?}")))
                    .h(px(26.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .when(active, |el| {
                        el.bg(theme.surface_raised).text_color(theme.text)
                    })
                    .when(!active, |el| {
                        el.hover(|style| style.text_color(theme.text))
                            .text_color(theme.text_muted)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter = entry;
                        cx.notify();
                    }))
                    .child(label)
            }))
            .into_any_element()
    }

    fn render_refresh(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let fetching = self.fetching;
        div()
            .id("integration-refresh")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.1)))
            .when(fetching, |el| el.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            .child(
                icon(icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .into_any_element()
    }

    /// Title, filters, and the grouped list. The closed page centers this
    /// column; an open worktree pins it to the left.
    fn render_inbox(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let header = self.render_header(theme, cx);
        let filters = self.render_filters(theme, cx);
        let list = self.render_list(theme, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .px(px(20.0))
                    .pt(px(24.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(header)
                    .child(filters),
            )
            .child(
                div()
                    .id("integration-inbox-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(div().px(px(16.0)).pt(px(16.0)).pb(px(28.0)).child(list)),
            )
            .into_any_element()
    }

    /// The inbox: worktrees grouped by what you can do with them. Trunk
    /// commits stay out — they belong to the open worktree's Commits tab.
    fn render_list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            let label = if self.loading {
                t!("integration.loading")
            } else if self.repo_path.is_none() {
                t!("integration.repo_none")
            } else {
                t!("integration.empty")
            };
            return self.list_placeholder(label, theme);
        };
        let layout = self
            .layout
            .as_ref()
            .cloned()
            .unwrap_or_else(|| graph::layout(&snapshot));
        let statuses = self.statuses(cx);
        let filter = self.filter;
        let trunk_name = snapshot.trunk.name.clone();
        let now = Utc::now();
        let all = [
            Filter::Blocked,
            Filter::Ready,
            Filter::Active,
            Filter::Merged,
        ];
        let one = [filter];
        let sections: &[Filter] = if filter == Filter::All { &all } else { &one };

        let mut groups: Vec<AnyElement> = Vec::new();
        for section in sections {
            let rows: Vec<AnyElement> = layout
                .rows
                .iter()
                .filter_map(|kind| {
                    let RowKind::Worktree(worktree_index) = kind else {
                        return None;
                    };
                    let worktree = snapshot.worktrees.get(*worktree_index)?;
                    let status = statuses
                        .get(&worktree.name)
                        .map(|(status, _)| *status)
                        .unwrap_or(WorktreeStatus::Missing);
                    if !filter_matches(*section, status) {
                        return None;
                    }
                    Some(self.render_worktree_row(worktree, &statuses, &trunk_name, now, theme, cx))
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            let count = rows.len();
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(4.0))
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(filter_label(*section))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(count.to_string())),
                            ),
                    )
                    .child(
                        div()
                            .rounded(px(12.0))
                            .bg(crate::theme::wash(0.045))
                            .p(px(4.0))
                            .flex()
                            .flex_col()
                            .children(rows),
                    )
                    .into_any_element(),
            );
        }

        if groups.is_empty() {
            let label = if snapshot.worktrees.is_empty() {
                t!("integration.empty")
            } else {
                t!("integration.list.filtered_empty")
            };
            return self.list_placeholder(label, theme);
        }

        div()
            .id("integration-list")
            .flex()
            .flex_col()
            .gap(px(22.0))
            .children(groups)
            .into_any_element()
    }

    fn list_placeholder(&self, label: std::borrow::Cow<'static, str>, theme: &Theme) -> AnyElement {
        div()
            .py(px(72.0))
            .flex()
            .items_center()
            .justify_center()
            .px(px(24.0))
            .text_size(px(13.0))
            .text_color(theme.text_faint)
            .text_center()
            .child(label)
            .into_any_element()
    }

    fn render_worktree_row(
        &mut self,
        worktree: &holt_proto::IntegrationWorktree,
        statuses: &std::collections::HashMap<String, (WorktreeStatus, bool)>,
        trunk: &str,
        now: chrono::DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (status, orphan) = statuses
            .get(&worktree.name)
            .copied()
            .unwrap_or((WorktreeStatus::Missing, false));
        let title: SharedString = match worktree.chat_id.as_deref() {
            Some(chat_id) => self
                .chat_title(chat_id, cx)
                .unwrap_or_else(|| SharedString::from(worktree.branch.clone().unwrap_or_default())),
            None => SharedString::from(
                worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| worktree.name.clone()),
            ),
        };
        let branch = worktree.branch.clone().unwrap_or_default();
        let show_branch = !branch.is_empty() && branch != title.as_ref();
        let age = worktree
            .commits
            .first()
            .and_then(|commit| chrono::DateTime::parse_from_rfc3339(&commit.authored_at).ok())
            .map(|time| crate::state::format_time_ago(time.with_timezone(&Utc), now))
            .unwrap_or_default();
        let sha = worktree
            .head_sha
            .as_deref()
            .filter(|sha| !sha.is_empty())
            .map(short_sha);
        div()
            .id(SharedString::from(format!(
                "integration-worktree-row-{}",
                worktree.name
            )))
            .debug_selector({
                let name = worktree.name.clone();
                move || format!("integration-worktree-row-{name}")
            })
            .px(px(12.0))
            .py(px(10.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(
                self.selected.as_deref() == Some(worktree.name.as_str()),
                |el| el.bg(crate::theme::wash(0.08)),
            )
            .hover(|style| style.bg(crate::theme::wash(0.06)))
            .on_click({
                let name = worktree.name.clone();
                cx.listener(move |this, _, _, cx| this.select_worktree(&name, cx))
            })
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(title),
                    )
                    .when(!age.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(theme.text_faint)
                                .child(age),
                        )
                    })
                    .when(!worktree.missing && worktree.ahead > 0, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(theme.success)
                                .child(format!("+{}", worktree.ahead)),
                        )
                    })
                    .when(!worktree.missing && worktree.behind > 0, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(theme.danger)
                                .child(format!("−{}", worktree.behind)),
                        )
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(Self::status_chip(status, trunk, theme))
                    .when(show_branch, |el| {
                        el.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(branch),
                        )
                    })
                    .when(orphan, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(t!("integration.orphan")),
                        )
                    })
                    .when_some(sha, |el, sha| {
                        el.child(
                            div()
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(sha),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The right column: the selected worktree's detail, or the empty
    /// prompt.
    fn render_detail(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = self.snapshot.clone();
        let (Some(snapshot), Some(selected)) = (&snapshot, self.selected.clone()) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .px(px(32.0))
                .text_center()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .child(t!("integration.detail.none")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(t!("integration.detail.none_hint")),
                )
                .into_any_element();
        };
        let Some(worktree) = snapshot.worktrees.iter().find(|w| w.name == selected) else {
            return div().size_full().into_any_element();
        };
        let title = match worktree.chat_id.as_deref() {
            Some(chat_id) => self
                .chat_title(chat_id, cx)
                .unwrap_or_else(|| SharedString::from(worktree.branch.clone().unwrap_or_default())),
            None => SharedString::from(
                worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| worktree.name.clone()),
            ),
        };
        let statuses = self.statuses(cx);
        let status = statuses
            .get(&worktree.name)
            .map(|(status, _)| *status)
            .unwrap_or(WorktreeStatus::Missing);
        let state = self.state.clone();
        self.ensure_checks_watch(cx);
        let checks_card = self.render_checks_card(theme, cx);
        let review_card = self.render_review_card(worktree, theme, cx);
        let review_line = self
            .review_summary()
            .unwrap_or_else(|| t!("integration.review.summary_none").into());
        let Some(detail) = self.detail.as_mut() else {
            return div().size_full().into_any_element();
        };
        let pane = detail.render(
            worktree,
            title,
            status,
            snapshot,
            &state,
            checks_card,
            review_card,
            review_line,
            theme,
            cx,
        );
        let bar = self.render_merge_bar(theme, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(pane)
            .child(bar)
            .into_any_element()
    }

    // ---- merge and delete actions (§7.5) ----

    /// The selected worktree's row, cloned out of the snapshot.
    fn selected_row(&self) -> Option<holt_proto::IntegrationWorktree> {
        let snapshot = self.snapshot.as_ref()?;
        let name = self.selected.as_ref()?;
        snapshot
            .worktrees
            .iter()
            .find(|worktree| &worktree.name == name)
            .cloned()
    }

    /// Click 合并: preview the mode in memory, then let the dialog (or the
    /// conflict row) take over.
    fn start_merge(&mut self, mode: MergeMode, cx: &mut Context<Self>) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(trunk) = self.trunk_of() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.merge_mode = mode;
        self.merge = Some(MergeFlow {
            worktree: name.clone(),
            state: MergeState::Previewing,
        });
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::PREVIEW_MERGE,
                    serde_json::json!({
                        "repoPath": repo_path,
                        "worktree": name.clone(),
                        "trunk": trunk,
                        "mode": mode.wire(),
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                let state = match result.and_then(|value| {
                    serde_json::from_value::<holt_proto::MergePreview>(value)
                        .map_err(|error| holt_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(preview) => match &preview.outcome {
                        holt_proto::MergeOutcome::Ok { message, .. } => {
                            let text = message.clone();
                            let input = cx.new(|cx| {
                                let mut input = ComposerInput::new(
                                    t!("integration.merge.message_placeholder"),
                                    cx,
                                );
                                input.set_text(text, cx);
                                input
                            });
                            MergeState::Dialog {
                                preview,
                                message: input,
                                diff_open: false,
                            }
                        }
                        holt_proto::MergeOutcome::Conflict { paths } => MergeState::Conflict {
                            paths: paths.clone(),
                        },
                        holt_proto::MergeOutcome::Refused { reason, detail } => {
                            MergeState::Refused {
                                reason: *reason,
                                detail: detail.clone(),
                            }
                        }
                    },
                    Err(error) => MergeState::Failed {
                        message: error.to_string().into(),
                    },
                };
                if let Some(flow) = &mut page.merge {
                    flow.state = state;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The confirm dialog's 合并: land it with the previewed heads and the
    /// (possibly edited) message.
    fn confirm_merge(&mut self, cx: &mut Context<Self>) {
        // Everything the flow owns is cloned out before `self` is touched
        // again — the flow itself is then flipped to Merging.
        let (worktree, heads, message) = {
            let Some(flow) = &self.merge else {
                return;
            };
            let MergeState::Dialog {
                preview, message, ..
            } = &flow.state
            else {
                return;
            };
            let worktree = flow.worktree.clone();
            let heads = preview.heads.clone();
            let message = if self.merge_mode == MergeMode::Squash {
                Some(message.read(cx).text().to_string())
            } else {
                None
            };
            (worktree, heads, message)
        };
        let repo_path = self.repo_path.clone();
        let trunk = self.trunk_of();
        let mode = self.merge_mode;
        let engine = self.state.read(cx).engine().cloned();
        let Some(repo_path) = repo_path else {
            return;
        };
        let Some(trunk) = trunk else {
            return;
        };
        let Some(engine) = engine else {
            return;
        };
        if let Some(flow) = &mut self.merge {
            flow.state = MergeState::Merging;
        }
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({
                "repoPath": repo_path,
                "worktree": worktree.clone(),
                "trunk": trunk,
                "mode": mode.wire(),
                "expect": heads,
            });
            if let Some(message) = &message {
                params["message"] = serde_json::Value::String(message.clone());
            }
            let result = engine
                .client()
                .call(methods::MERGE_INTO_TRUNK, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<holt_proto::MergeResult>(value)
                        .map_err(|error| holt_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(result) => {
                        page.merge = None;
                        cx.emit(IntegrationEvent::Notice {
                            message: t!(
                                "integration.merge.merged_toast",
                                sha = short_sha(&result.merged_sha)
                            )
                            .into_owned(),
                            success: true,
                        });
                        page.load(cx);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let refusal = message.lines().next().and_then(|first| {
                            first
                                .strip_prefix("merge-refused: ")
                                .and_then(holt_proto::MergeRefusal::parse_wire)
                        });
                        match refusal {
                            // The trunk or branch moved under the dialog: a
                            // fresh preview is the honest next frame.
                            Some(holt_proto::MergeRefusal::Stale) => {
                                page.merge = None;
                                page.start_merge(mode, cx);
                            }
                            Some(reason) => {
                                let detail = message.lines().skip(1).map(str::to_string).collect();
                                if let Some(flow) = &mut page.merge {
                                    flow.state = MergeState::Refused { reason, detail };
                                }
                            }
                            None => {
                                if let Some(flow) = &mut page.merge {
                                    flow.state = MergeState::Failed {
                                        message: message.into(),
                                    };
                                }
                            }
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn cancel_merge(&mut self, cx: &mut Context<Self>) {
        self.merge = None;
        cx.notify();
    }

    /// Open the "hand the conflicts to the author chat" dialog with the
    /// default instruction text.
    fn open_resolve(&mut self, paths: &[String], cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(trunk) = self.trunk_of() else {
            return;
        };
        let mut body = t!("integration.merge.resolve_prefix", trunk = trunk.as_str()).into_owned();
        body.push('\n');
        for path in paths {
            body.push_str(path);
            body.push('\n');
        }
        body.push_str(&t!("integration.merge.resolve_suffix"));
        self.resolve = Some(ResolveDialog {
            worktree: row.name.clone(),
            input: cx.new(|cx| {
                let mut input = ComposerInput::new("", cx);
                input.set_text(body, cx);
                input
            }),
        });
        cx.notify();
    }

    /// Queue the (edited) resolution instruction into the author chat.
    fn send_resolution(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.resolve.take() else {
            return;
        };
        let prompt = dialog.input.read(cx).text().to_string();
        let Some(row) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.name == dialog.worktree)
            })
            .cloned()
        else {
            return;
        };
        let Some(chat_id) = row.chat_id.clone() else {
            return;
        };
        let (config, cwd) = {
            let state = self.state.read(cx);
            let chat = state.chat_row(&chat_id);
            match chat.map(|chat| (chat.config.clone(), chat.cwd.clone())) {
                Some((config, cwd)) => (config, cwd),
                None => (None, None),
            }
        };
        let Some(config) = config else {
            cx.emit(IntegrationEvent::Notice {
                message: t!("integration.merge.no_model").into_owned(),
                success: false,
            });
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let cwd = cwd.unwrap_or_else(|| row.path.clone());
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let request = serde_json::json!({
                "prompt": prompt,
                "provider": config.provider,
                "model": config.model,
                "reasoning": config.reasoning,
                "modelOptions": config.model_options,
                "cwd": cwd,
                "permissionMode": config.permission_mode,
            });
            let result = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id,
                        "command": {
                            "kind": "run",
                            "messageId": uuid::Uuid::new_v4().to_string(),
                            "request": request,
                        },
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.merge = None;
                        cx.emit(IntegrationEvent::Notice {
                            message: t!("integration.merge.resolve_sent").into_owned(),
                            success: true,
                        });
                    }
                    Err(error) => {
                        cx.emit(IntegrationEvent::Notice {
                            message: error.to_string(),
                            success: false,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        self.delete_confirm = Some(DeleteConfirm {
            dirty: row.dirty,
            worktree: row.name.clone(),
        });
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.delete_confirm.take() else {
            return;
        };
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.deleting = Some(confirm.worktree.clone());
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_WORKTREE,
                    serde_json::json!({
                        "repoPath": repo_path,
                        "worktree": confirm.worktree.clone(),
                        "force": confirm.dirty,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.deleting = None;
                match result {
                    Ok(_) => {
                        cx.emit(IntegrationEvent::Notice {
                            message: t!("integration.delete.done_toast").into_owned(),
                            success: true,
                        });
                        page.merge = None;
                        page.load(cx);
                    }
                    Err(error) => {
                        cx.emit(IntegrationEvent::Notice {
                            message: error.to_string(),
                            success: false,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- checks (§8.4) ----

    /// Start (or keep) the checks watch for the selected worktree. Called
    /// from render — guarded, cheap, and the pane is lazily created.
    fn ensure_checks_watch(&mut self, cx: &mut Context<Self>) {
        if self.checks_watch.is_some() {
            return;
        }
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(worktree) = self.selected.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.checks_watch = Some(cx.spawn(async move |this, cx| {
            let subscribed = engine
                .client()
                .subscribe(
                    methods::WATCH_CHECKS,
                    serde_json::json!({ "repoPath": repo_path, "worktree": worktree }),
                )
                .await;
            let Ok(mut stream) = subscribed else {
                return;
            };
            while let Some(frame) = stream.recv().await {
                let decoded = if frame.is_null() {
                    Ok(None)
                } else {
                    serde_json::from_value::<holt_rpc::checks::CheckRun>(frame.clone()).map(Some)
                };
                let Ok(decoded) = decoded else {
                    continue;
                };
                let alive = this.update(cx, |page, cx| {
                    page.checks_error = None;
                    page.checks_run = decoded;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// The 运行 / 重新运行 button's action.
    fn run_checks(&mut self, cx: &mut Context<Self>) {
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(worktree) = self.selected.clone() else {
            return;
        };
        let Some(trunk) = self.trunk_of() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.checks_error = None;
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RUN_CHECKS,
                    serde_json::json!({
                        "repoPath": repo_path,
                        "worktree": worktree,
                        "trunk": trunk,
                    }),
                )
                .await;
            if let Err(error) = result {
                this.update(cx, |page, cx| {
                    page.checks_error = Some(error.to_string().into());
                    cx.notify();
                })
                .ok();
            }
            // Success needs nothing here: the watch delivers the frames.
        }));
    }

    /// Toggle one check's live log.
    fn toggle_check_log(&mut self, name: &str, run_id: &str, cx: &mut Context<Self>) {
        if self
            .checks_log
            .as_ref()
            .is_some_and(|(open, _)| open == name)
        {
            self.checks_log = None;
            cx.notify();
            return;
        }
        let view = cx.new(|_| CheckLogView {
            lines: Vec::new(),
            _task: None,
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let run_id = run_id.to_string();
        let name_owned = name.to_string();
        view.update(cx, |view, cx| {
            view._task = Some(cx.spawn(async move |this, cx| {
                let Ok(mut stream) = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_CHECK_LOG,
                        serde_json::json!({
                            "runId": run_id,
                            "name": name_owned,
                            "afterSeq": 0,
                        }),
                    )
                    .await
                else {
                    return;
                };
                loop {
                    let Some(event) = stream.recv().await else {
                        return;
                    };
                    let line = check_log_line(&event);
                    let alive = this.update(cx, |view: &mut CheckLogView, cx| {
                        view.lines.push(line.clone());
                        if view.lines.len() > 2000 {
                            view.lines.drain(..view.lines.len() - 2000);
                        }
                        cx.notify();
                    });
                    if event.get("exit").is_some() || alive.is_err() {
                        return;
                    }
                }
            }));
        });
        self.checks_log = Some((name.to_string(), view));
        cx.notify();
    }

    // ---- review (§9.6) ----

    /// Fetch (once per selection) the reviewer catalog, and keep the reviews
    /// list fresh for the selected worktree (a chats change re-arms it).
    fn ensure_review_watch(&mut self, cx: &mut Context<Self>) {
        if self.reviews_watch.is_some() {
            return;
        }
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(worktree) = self.selected.clone() else {
            return;
        };
        let Some(trunk) = self.trunk_of() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.reviewers.is_empty() {
            let engine = engine.clone();
            let repo_path = repo_path.clone();
            let trunk = trunk.clone();
            cx.spawn(async move |this, cx| {
                let listed = engine
                    .client()
                    .call(
                        methods::LIST_REVIEWERS,
                        serde_json::json!({ "repoPath": repo_path, "trunk": trunk }),
                    )
                    .await;
                let _ = this.update(cx, |page, cx| {
                    if let Ok(value) = listed {
                        page.reviewers = serde_json::from_value::<Vec<serde_json::Value>>(value)
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|row| {
                                Some((
                                    row.get("name")?.as_str()?.to_string(),
                                    row.get("source")?.as_str()?.to_string(),
                                ))
                            })
                            .collect();
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        self.reviews_watch = Some(cx.spawn(async move |this, cx| {
            let listed = engine
                .client()
                .call(
                    methods::LIST_REVIEWS,
                    serde_json::json!({
                        "repoPath": repo_path,
                        "worktree": worktree,
                        "trunk": trunk,
                    }),
                )
                .await;
            let _ = this.update(cx, |page, cx| {
                if let Ok(value) = listed {
                    page.reviews = enrich_reviews(
                        serde_json::from_value::<Vec<serde_json::Value>>(value).unwrap_or_default(),
                        &page.state,
                        cx,
                    );
                    page.reviews_loaded = true;
                }
                cx.notify();
            });
        }));
    }

    /// Start a review for the selected worktree. `custom` carries the
    /// one-off prompt of the 自定义 entry. The model is the author chat's
    /// (engine-side); a worktree with no author chat takes the composer's
    /// resolved model.
    fn start_review(&mut self, reviewer: &str, custom: Option<String>, cx: &mut Context<Self>) {
        let Some(repo_path) = self.repo_path.clone() else {
            return;
        };
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(trunk) = self.trunk_of() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let reviewer_label = if custom.is_some() { "custom" } else { reviewer };
        let branch = row.branch.clone().unwrap_or_else(|| row.name.clone());
        let mut params = serde_json::json!({
            "repoPath": repo_path,
            "worktree": row.name,
            "trunk": trunk,
            "reviewer": reviewer,
            "title": t!(
                "integration.review.chat_title",
                branch = branch.as_str(),
                reviewer = reviewer_label
            ),
        });
        if let Some(prompt) = custom {
            params["customPrompt"] = serde_json::Value::String(prompt);
        }
        let has_author = row
            .chat_id
            .as_ref()
            .is_some_and(|chat_id| self.state.read(cx).chat_row(chat_id).is_some());
        if !has_author {
            let Some(model) = self.pickers.read(cx).resolved(cx).model else {
                cx.emit(IntegrationEvent::Notice {
                    message: t!("integration.review.no_model").into_owned(),
                    success: false,
                });
                return;
            };
            params["model"] = serde_json::Value::String(model);
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::START_REVIEW, params).await;
            if let Err(error) = result {
                this.update(cx, |_, cx| {
                    cx.emit(IntegrationEvent::Notice {
                        message: error.to_string(),
                        success: false,
                    });
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Open the custom-review dialog.
    fn open_custom_review(&mut self, cx: &mut Context<Self>) {
        self.review_dialog = Some(ReviewDialog::Custom {
            input: cx.new(|cx| ComposerInput::new("", cx)),
        });
        cx.notify();
    }

    /// Open the send-back dialog seeded with `findings` (the selection, or
    /// the whole latest review).
    fn open_send_back(
        &mut self,
        reviewer: &str,
        findings: &[serde_json::Value],
        cx: &mut Context<Self>,
    ) {
        let Some(chat_id) = self.selected_row().and_then(|row| row.chat_id.clone()) else {
            return;
        };
        let body = send_back_body(reviewer, findings);
        self.review_dialog = Some(ReviewDialog::Send {
            chat_id,
            input: cx.new(|cx| {
                let mut input = ComposerInput::new("", cx);
                input.set_text(body, cx);
                input
            }),
        });
        cx.notify();
    }

    /// Confirm the open review dialog: start the custom review, or queue
    /// the edited send-back into the author chat.
    fn confirm_review_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.review_dialog.take() else {
            return;
        };
        match dialog {
            ReviewDialog::Custom { input } => {
                let prompt = input.read(cx).text().to_string();
                if prompt.trim().is_empty() {
                    self.review_dialog = Some(ReviewDialog::Custom { input });
                    return;
                }
                self.start_review("custom", Some(prompt), cx);
            }
            ReviewDialog::Send { chat_id, input } => {
                let prompt = input.read(cx).text().to_string();
                self.queue_into_author(chat_id, prompt, cx);
                self.selected_findings.clear();
            }
        }
        cx.notify();
    }

    /// Queue `prompt` into the author chat with its own config and cwd.
    fn queue_into_author(&mut self, chat_id: String, prompt: String, cx: &mut Context<Self>) {
        let (config, cwd) = {
            let state = self.state.read(cx);
            match state.chat_row(&chat_id) {
                Some(chat) => (chat.config.clone(), chat.cwd.clone()),
                None => (None, None),
            }
        };
        let Some(config) = config else {
            cx.emit(IntegrationEvent::Notice {
                message: t!("integration.merge.no_model").into_owned(),
                success: false,
            });
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let cwd = cwd.or_else(|| self.selected_row().map(|row| row.path.clone()));
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    serde_json::json!({
                        "chatId": chat_id,
                        "command": {
                            "kind": "run",
                            "messageId": uuid::Uuid::new_v4().to_string(),
                            "request": {
                                "prompt": prompt,
                                "provider": config.provider,
                                "model": config.model,
                                "reasoning": config.reasoning,
                                "modelOptions": config.model_options,
                                "cwd": cwd,
                                "permissionMode": config.permission_mode,
                            },
                        },
                    }),
                )
                .await;
            this.update(cx, |_, cx| {
                cx.emit(match result {
                    Ok(_) => IntegrationEvent::Notice {
                        message: t!("integration.review.sent").into_owned(),
                        success: true,
                    },
                    Err(error) => IntegrationEvent::Notice {
                        message: error.to_string(),
                        success: false,
                    },
                });
            })
            .ok();
        }));
    }

    /// The review the card shows: a running one first, else the newest
    /// settled one (submitted or incomplete).
    fn latest_review(&self) -> Option<&serde_json::Value> {
        self.reviews.first()
    }

    /// The review card: reviewer buttons, the latest findings, and the
    /// history list.
    fn render_review_card(
        &mut self,
        worktree: &holt_proto::IntegrationWorktree,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_review_watch(cx);
        let mut buttons: Vec<AnyElement> = self
            .reviewers
            .iter()
            .map(|(name, source)| {
                let label = if source == "repo" {
                    format!("{name} ·")
                } else {
                    name.clone()
                };
                let name = name.clone();
                popover::btn_primary(theme, &label)
                    .id(SharedString::from(format!("integration-reviewer-{name}")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_review(&name, None, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        buttons.push(
            popover::btn_ghost(
                theme,
                &t!("integration.review.custom"),
                "integration-reviewer-custom",
            )
            .id("integration-reviewer-custom")
            .on_click(cx.listener(|this, _, _, cx| this.open_custom_review(cx)))
            .into_any_element(),
        );

        let latest = self.latest_review().cloned();
        let has_author = worktree.chat_id.is_some();
        let findings_list: Vec<serde_json::Value> = latest
            .as_ref()
            .and_then(|review| review["findings"].as_array().cloned())
            .unwrap_or_default();
        // Grouped by severity: high, medium, low.
        let mut ordered = findings_list.clone();
        ordered.sort_by_key(|finding| match finding["severity"].as_str() {
            Some("high") => 0,
            Some("medium") => 1,
            _ => 2,
        });
        let findings: Vec<AnyElement> = ordered
            .iter()
            .map(|finding| {
                let id = finding["id"].as_str().unwrap_or_default().to_string();
                let severity = finding["severity"].as_str().unwrap_or("?");
                let color = match severity {
                    "high" => theme.danger,
                    "medium" => theme.warning,
                    _ => theme.text_muted,
                };
                let path = finding["path"].as_str().unwrap_or("?").to_string();
                let location = match finding["line"].as_u64() {
                    Some(line) => format!("{path}:{line}"),
                    None => path.clone(),
                };
                let selected = self.selected_findings.contains(&id);
                let toggle_id = id.clone();
                div()
                    .id(SharedString::from(format!("integration-finding-{id}")))
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .px(px(16.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(theme.border.opacity(0.4))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.04)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(detail) = &mut this.detail {
                            detail.reveal_in_changes(path.clone());
                        }
                        cx.notify();
                    }))
                    .when(has_author, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("integration-finding-pick-{id}")))
                                .flex_none()
                                .w(px(14.0))
                                .h(px(14.0))
                                .mt(px(1.0))
                                .rounded(px(3.0))
                                .border_1()
                                .border_color(theme.border)
                                .when(selected, |el| el.bg(theme.accent))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.selected_findings.remove(&toggle_id) {
                                        this.selected_findings.insert(toggle_id.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(46.0))
                            .text_size(px(10.5))
                            .text_color(color)
                            .child(severity.to_string()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.0))
                                    .text_color(theme.text_dim)
                                    .child(location),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text)
                                    .child(finding["title"].as_str().unwrap_or("").to_string()),
                            )
                            .when_some(
                                finding["detail"]
                                    .as_str()
                                    .filter(|detail| !detail.is_empty())
                                    .map(str::to_string),
                                |el, detail| {
                                    el.child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(theme.text_muted)
                                            .child(detail),
                                    )
                                },
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        // Actions under the findings: send back (selection or all), open
        // the review chat.
        let mut actions: Vec<AnyElement> = Vec::new();
        if let Some(review) = &latest {
            let reviewer = review["reviewer"].as_str().unwrap_or_default().to_string();
            if has_author && !findings_list.is_empty() {
                let picked: Vec<serde_json::Value> = findings_list
                    .iter()
                    .filter(|finding| {
                        finding["id"]
                            .as_str()
                            .is_some_and(|id| self.selected_findings.contains(id))
                    })
                    .cloned()
                    .collect();
                let label = if picked.is_empty() {
                    t!("integration.review.send_back").into_owned()
                } else {
                    t!("integration.review.send_selected", count = picked.len()).into_owned()
                };
                let send = if picked.is_empty() {
                    findings_list.clone()
                } else {
                    picked
                };
                actions.push(
                    popover::btn_primary(theme, &label)
                        .id("integration-review-send-back")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_send_back(&reviewer, &send, cx);
                        }))
                        .into_any_element(),
                );
            }
            if let Some(chat_id) = review["chatId"].as_str().map(str::to_string) {
                actions.push(
                    popover::btn_ghost(
                        theme,
                        &t!("integration.review.open_chat"),
                        "integration-review-open-chat",
                    )
                    .id("integration-review-open-chat")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(IntegrationEvent::OpenChat(chat_id.clone()));
                    }))
                    .into_any_element(),
                );
            }
        }

        let history_rows: Vec<AnyElement> = self
            .reviews
            .iter()
            .map(|review| {
                let stale = review["stale"] == serde_json::json!(true);
                let (label, color) = review_state_label(review, theme);
                let chat_id = review["chatId"].as_str().unwrap_or("?").to_string();
                let open_id = chat_id.clone();
                div()
                    .id(SharedString::from(format!(
                        "integration-review-history-{chat_id}"
                    )))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(16.0))
                    .h(px(28.0))
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.04)))
                    .when(stale, |el| el.opacity(0.5))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(IntegrationEvent::OpenChat(open_id.clone()));
                    }))
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme.text_muted)
                            .child(review["reviewer"].as_str().unwrap_or("?").to_string()),
                    )
                    .child(div().text_color(color).child(label))
                    .when(stale, |el| {
                        el.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.warning)
                                .child(t!("integration.review.stale")),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        div()
            .mt(px(12.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .debug_selector(|| "integration-review-card".into())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(16.0))
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(t!("integration.review.title"))
                    .child(div().flex_1())
                    .children(buttons),
            )
            .when(latest.is_none(), |el| {
                el.child(
                    div()
                        .px(px(16.0))
                        .pb(px(12.0))
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(t!("integration.review.none")),
                )
            })
            .children(findings)
            .when(!actions.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .px(px(16.0))
                        .py(px(8.0))
                        .children(actions),
                )
            })
            .when(!history_rows.is_empty(), |el| {
                el.child(
                    div()
                        .mt(px(6.0))
                        .border_t_1()
                        .border_color(theme.border.opacity(0.6))
                        .children(history_rows),
                )
            })
            .into_any_element()
    }

    /// The custom-review / send-back dialog.
    fn render_review_dialog(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(dialog) = &self.review_dialog else {
            return div().into_any_element();
        };
        let (title, body, confirm, input) = match dialog {
            ReviewDialog::Custom { input } => (
                t!("integration.review.custom_title"),
                t!("integration.review.custom_body"),
                t!("integration.review.custom_start"),
                input.clone(),
            ),
            ReviewDialog::Send { input, .. } => (
                t!("integration.review.send_title"),
                t!("integration.review.send_body"),
                t!("integration.review.send_confirm"),
                input.clone(),
            ),
        };
        let card = popover::dialog_card(theme)
            .w(px(560.0))
            .id("integration-review-dialog-card")
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.review_dialog = None;
                cx.notify();
            }))
            .child(popover::dialog_title(theme, &title))
            .child(
                div()
                    .mt(px(10.0))
                    .px(px(18.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(body),
            )
            .child(div().mt(px(10.0)).mx(px(18.0)).child(input))
            .child(
                div()
                    .mt(px(14.0))
                    .mb(px(16.0))
                    .mx(px(18.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            theme,
                            &t!("integration.merge.cancel"),
                            "integration-review-dialog-cancel",
                        )
                        .id("integration-review-dialog-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.review_dialog = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        popover::btn_primary(theme, &confirm)
                            .id("integration-review-dialog-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_review_dialog(cx))),
                    ),
            );
        popover::modal(
            "integration-review-dialog",
            window.viewport_size(),
            card.into_any_element(),
        )
    }

    /// The checks card, shown on the Summary tab.
    fn render_checks_card(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let run = self.checks_run.clone();
        let error = self.checks_error.clone();
        let running = run.as_ref().is_some_and(|run| !run.finished());
        let stale = run.as_ref().is_some_and(|run| run.stale);
        let open_log = self.checks_log.as_ref().map(|(name, _)| name.clone());

        let body: AnyElement = match (&run, &error) {
            (None, None) => div()
                .py(px(8.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(t!("integration.checks.none"))
                .into_any_element(),
            (_, Some(error)) => div()
                .py(px(8.0))
                .text_size(px(12.0))
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element(),
            (Some(run), None) => {
                let rows: Vec<AnyElement> = run
                    .checks
                    .iter()
                    .map(|row| {
                        let (glyph, color): (&'static str, _) = match row.status.as_str() {
                            "passed" => ("✓", theme.success),
                            "failed" => ("✕", theme.danger),
                            "timeout" => ("⏱", theme.warning),
                            "running" => ("…", theme.busy),
                            "stopped" => ("■", theme.text_faint),
                            _ => ("·", theme.text_faint),
                        };
                        let name = row.name.clone();
                        let is_open = open_log.as_deref() == Some(row.name.as_str());
                        div()
                            .id(SharedString::from(format!(
                                "integration-check-row-{}",
                                row.name
                            )))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .h(px(30.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::ink(0.025)))
                            .on_click({
                                let run_id = run.run_id.clone();
                                let name = name.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_check_log(&name, &run_id, cx);
                                })
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(14.0))
                                    .text_size(px(11.0))
                                    .text_color(color)
                                    .child(glyph),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(name),
                            )
                            .when_some(row.duration_ms, |el, ms| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_faint)
                                        .child(format_duration_ms(ms)),
                                )
                            })
                            .when(is_open, |el| {
                                el.child(
                                    icon(icons::ALT_ARROW_DOWN)
                                        .size(px(11.0))
                                        .text_color(theme.text_faint),
                                )
                            })
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(run.error.is_some(), |el| {
                        el.child(
                            div()
                                .py(px(8.0))
                                .text_size(px(12.0))
                                .text_color(theme.danger)
                                .child(run.error.clone().unwrap_or_default()),
                        )
                    })
                    .when_some(
                        open_log.and_then(|_| self.checks_log.as_ref().map(|(_, v)| v.clone())),
                        |el, view| el.child(div().max_h(px(220.0)).child(view)),
                    )
                    .into_any_element()
            }
        };

        div()
            .id("integration-checks-card")
            .debug_selector(|| "integration-checks-card".into())
            .mt(px(22.0))
            .when(stale, |el| el.opacity(0.55))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(32.0))
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(div().flex_none().child(t!("integration.checks.title")))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(t!("integration.checks.subtitle")),
                    )
                    .when(stale, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(theme.warning)
                                .child(t!("integration.checks.stale")),
                        )
                    })
                    .child(
                        div()
                            .id("integration-checks-run")
                            .debug_selector(|| "integration-checks-run".into())
                            .flex_none()
                            .h(px(26.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .hover(|style| style.bg(crate::theme::wash(0.08)))
                            .when(running, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.run_checks(cx)))
                            .child(t!("integration.checks.rerun")),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    /// The merge bar's one-line review summary (never a gate).
    fn review_summary(&self) -> Option<SharedString> {
        if !self.reviews_loaded {
            return None;
        }
        let Some(review) = self.reviews.first() else {
            return Some(t!("integration.review.summary_none").into());
        };
        let state = review["state"].as_str().unwrap_or("");
        if state == "running" {
            return Some(t!("integration.review.summary_running").into());
        }
        if review["stale"] == serde_json::json!(true) {
            return Some(t!("integration.review.summary_stale").into());
        }
        if state == "incomplete" {
            return Some(t!("integration.review.summary_incomplete").into());
        }
        let count = review["findingCount"].as_u64().unwrap_or(0);
        if review["verdict"] == "clean" {
            Some(t!("integration.review.summary_clean").into())
        } else {
            Some(t!("integration.review.summary_issues", count = count).into())
        }
    }

    /// The merge bar's one-line checks summary (never a gate).
    fn checks_summary(&self) -> Option<SharedString> {
        let run = self.checks_run.as_ref()?;
        if let Some(error) = &run.error {
            let _ = error;
            return Some(t!("integration.checks.summary_error").into());
        }
        if run.stale {
            return Some(t!("integration.checks.summary_stale").into());
        }
        if !run.finished() {
            return Some(t!("integration.checks.summary_running").into());
        }
        let failed = run
            .checks
            .iter()
            .filter(|row| !matches!(row.status.as_str(), "passed"))
            .count();
        if failed == 0 {
            Some(t!("integration.checks.summary_passed").into())
        } else {
            Some(t!("integration.checks.summary_failed", count = failed).into())
        }
    }

    /// The detail's bottom bar (§7.5): what can happen to the selected
    /// worktree right now. Checks and reviews are P2/P3 — nothing here ever
    /// blocks on them.
    fn render_merge_bar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return div().into_any_element();
        };
        let Some(row) = self.selected_row() else {
            return div().into_any_element();
        };
        let statuses = self.statuses(cx);
        let Some((status, _)) = statuses.get(&row.name) else {
            return div().into_any_element();
        };
        let trunk = snapshot.trunk.name.clone();
        let delete_hidden = delete_protected(
            row.branch.as_deref(),
            trunk.as_str(),
            snapshot.main_checkout.branch.as_deref(),
        );
        let busy = self
            .deleting
            .as_deref()
            .is_some_and(|name| name == row.name);

        // An in-flight flow for THIS worktree replaces the plain bar.
        let flow_active = self
            .merge
            .as_ref()
            .is_some_and(|flow| flow.worktree == row.name);
        let bar: AnyElement = if flow_active {
            let flow_state = self
                .merge
                .as_ref()
                .map(|flow| flow.state.clone())
                .expect("checked above");
            match &flow_state {
                MergeState::Previewing | MergeState::Merging => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(crate::loaders::mini_glyph_spinner(
                        "integration-merge-spinner",
                        1.75,
                        theme.glyph,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(t!("integration.merge.working"))
                    .into_any_element(),
                MergeState::Dialog { .. } => div().into_any_element(),
                MergeState::Conflict { paths } => self.render_conflict_bar(&row, paths, theme, cx),
                MergeState::Refused { reason, detail } => {
                    self.render_refused_bar(&snapshot, *reason, detail, theme, cx)
                }
                MergeState::Failed { message } => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .child(message.clone())
                    .into_any_element(),
            }
        } else {
            let bar = match status {
                WorktreeStatus::Working => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!("integration.merge.agent_working"))
                    .child(div().flex_1())
                    .child(disabled_primary(
                        theme,
                        &t!("integration.merge.merge_into", trunk = trunk.as_str()),
                    )),
                WorktreeStatus::Dirty => {
                    let hint: SharedString = if row.dirty_files > 0 {
                        t!(
                            "integration.merge.dirty_hint_files",
                            count = row.dirty_files
                        )
                        .into()
                    } else {
                        t!("integration.merge.dirty_hint").into()
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.warning)
                        .child(hint)
                        .child(div().flex_1())
                        .child(self.render_mode_toggle(&row, theme, cx))
                        .child(primary_merge_button(theme, cx))
                }
                WorktreeStatus::Merged => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!(
                        "integration.merge.merged_hint",
                        trunk = trunk.as_str(),
                        sha = short_sha(row.merge_sha.as_deref().unwrap_or(""))
                    ))
                    .when(!delete_hidden, |el| el.child(div().flex_1()))
                    .when(delete_hidden, |el| {
                        el.child(t!("integration.merge.delete_blocked_hint"))
                    })
                    .when(!delete_hidden, |el| {
                        el.child(
                            popover::btn_primary(
                                theme,
                                &t!("integration.merge.delete_worktree_branch"),
                            )
                            .id("integration-delete-wt-branch")
                            .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
                        )
                    }),
                WorktreeStatus::NoChanges => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!("integration.merge.no_changes_hint"))
                    .when(delete_hidden, |el| {
                        el.child(t!("integration.merge.delete_blocked_hint"))
                    })
                    .when(!delete_hidden, |el| {
                        el.child(div().flex_1()).child(
                            popover::btn_primary(theme, &t!("integration.merge.delete_worktree"))
                                .id("integration-delete-wt")
                                .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
                        )
                    }),
                WorktreeStatus::Missing => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!("integration.merge.missing_hint"))
                    .child(div().flex_1())
                    .child(
                        popover::btn_primary(theme, &t!("integration.merge.cleanup"))
                            .id("integration-cleanup-wt")
                            .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
                    ),
                WorktreeStatus::Behind => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!("integration.merge.behind_hint", trunk = trunk.as_str()))
                    .child(div().flex_1())
                    .child(primary_merge_button(theme, cx)),
                WorktreeStatus::Ready => div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(self.render_mode_toggle(&row, theme, cx))
                    .child(div().flex_1())
                    .child(primary_merge_button(theme, cx)),
            };
            bar.into_any_element()
        };

        let busy_row = busy.then(|| {
            div()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(t!("integration.delete.working"))
        });
        // The review summary rides the bar beside the checks line — also
        // never a gate.
        let review_line = self.review_summary().map(|summary| {
            div()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(summary)
        });
        // The checks summary rides the bar as a hint — never a gate.
        let checks_line = self.checks_summary().map(|summary| {
            div()
                .text_size(px(11.0))
                .text_color(
                    if self.checks_run.as_ref().is_some_and(|run| {
                        run.finished() && run.checks.iter().any(|row| row.status != "passed")
                    }) {
                        theme.warning
                    } else {
                        theme.text_faint
                    },
                )
                .child(summary)
        });
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .mt(px(10.0))
            .pt(px(10.0))
            .border_t_1()
            .border_color(theme.border.opacity(0.6))
            .child(bar)
            .children(checks_line)
            .children(review_line)
            .children(busy_row)
            .into_any_element()
    }

    /// The conflict row of the merge bar: N files, the editor escape
    /// hatch, and (with an owning chat) the hand-it-back dialog.
    fn render_conflict_bar(
        &mut self,
        row: &holt_proto::IntegrationWorktree,
        paths: &[String],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor_button = {
            let path = row.path.clone();
            popover::btn_primary(theme, &t!("integration.merge.open_editor"))
                .id("integration-merge-open-editor")
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(IntegrationEvent::OpenExternally(path.clone()));
                }))
        };
        let resolve_button = row.chat_id.is_some().then(|| {
            let paths = paths.to_vec();
            popover::btn_primary(theme, &t!("integration.merge.resolve_in_chat"))
                .id("integration-merge-resolve")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_resolve(&paths, cx);
                }))
        });
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.0))
            .text_size(px(12.0))
            .text_color(theme.danger)
            .child(t!("integration.merge.conflict_hint", count = paths.len()))
            .child(div().flex_1())
            .child(editor_button)
            .children(resolve_button)
            .into_any_element()
    }

    /// A refusal from preview or merge: the reason, up to eight detail
    /// lines, and the one action the reason suggests.
    fn render_refused_bar(
        &mut self,
        snapshot: &IntegrationSnapshot,
        reason: holt_proto::MergeRefusal,
        detail: &[String],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_main = (reason == holt_proto::MergeRefusal::TrunkCheckedOutDirty).then(|| {
            let path = snapshot.main_checkout.path.clone();
            popover::btn_primary(theme, &t!("integration.merge.open_main_checkout"))
                .id("integration-merge-open-main")
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(IntegrationEvent::OpenExternally(path.clone()));
                }))
        });
        let detail_rows: Vec<AnyElement> = detail
            .iter()
            .take(8)
            .map(|line| {
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(line.clone())
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.0))
                    .text_color(theme.warning)
                    .child(refusal_label(reason))
                    .child(div().flex_1())
                    .children(open_main),
            )
            .when(!detail_rows.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(px(2.0)).children(detail_rows))
            })
            .into_any_element()
    }

    /// Squash ⇄ Fast-forward. ff is offered but disabled while the branch
    /// is behind — the bar keeps its shape instead of losing a control.
    fn render_mode_toggle(
        &mut self,
        row: &holt_proto::IntegrationWorktree,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.merge_mode;
        let ff_disabled = row.behind > 0;
        div()
            .id("integration-merge-modes")
            .flex()
            .items_center()
            .child(self.mode_chip(MergeMode::Squash, selected, false, theme, cx))
            .child(self.mode_chip(MergeMode::FastForward, selected, ff_disabled, theme, cx))
            .when(ff_disabled, |el| {
                let hint = t!("integration.merge.ff_blocked", trunk = row.behind).into_owned();
                el.tooltip(move |_, cx| {
                    cx.new(|_| popover::TextTooltip(hint.clone().into())).into()
                })
            })
            .into_any_element()
    }

    /// The merge confirm dialog: mode, stats, the main checkout's state, an
    /// editable message, and the final diff behind a fold.
    fn render_merge_dialog(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((preview, message, diff_open)) =
            self.merge.as_ref().and_then(|flow| match &flow.state {
                MergeState::Dialog {
                    preview,
                    message,
                    diff_open,
                } => Some((preview.clone(), message.clone(), *diff_open)),
                _ => None,
            })
        else {
            return div().into_any_element();
        };
        let _ = window;
        let mode = self.merge_mode;
        let snapshot = self.snapshot.clone();
        let (trunk, branch, main_state) = match &snapshot {
            Some(snapshot) => (
                snapshot.trunk.name.clone(),
                self.selected_row()
                    .and_then(|row| row.branch)
                    .unwrap_or_default(),
                main_checkout_state(snapshot, theme),
            ),
            None => (String::new(), String::new(), None),
        };
        let stats = match &preview.outcome {
            holt_proto::MergeOutcome::Ok {
                files,
                truncated,
                additions,
                deletions,
                ..
            } => Some((
                t!(
                    "integration.merge.stats",
                    files = files.len(),
                    additions = additions,
                    deletions = deletions
                ),
                *truncated,
            )),
            _ => None,
        };
        let patch = match &preview.outcome {
            holt_proto::MergeOutcome::Ok { patch, .. } => patch.clone(),
            _ => String::new(),
        };

        let card = popover::dialog_card(theme)
            .id("integration-merge-dialog-card")
            .debug_selector(|| "integration-merge-dialog-card".into())
            .w(px(560.0))
            .max_h(px(560.0))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_merge(cx)))
            .child(popover::dialog_title(
                theme,
                &t!(
                    "integration.merge.dialog_title",
                    branch = branch.as_str(),
                    trunk = trunk.as_str()
                ),
            ))
            .child(
                div()
                    .mt(px(10.0))
                    .px(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .text_size(px(12.0))
                    .child(kv_row(
                        theme,
                        &t!("integration.merge.row_mode"),
                        mode.label().into_owned(),
                    ))
                    .when_some(stats, |el, (stats, truncated)| {
                        el.child(kv_row(
                            theme,
                            &t!("integration.merge.row_changes"),
                            format!(
                                "{stats}{}",
                                if truncated {
                                    t!("integration.merge.truncated_suffix").into_owned()
                                } else {
                                    String::new()
                                }
                            ),
                        ))
                    })
                    .when_some(main_state, |el, state| {
                        el.child(kv_row(
                            theme,
                            &t!("integration.merge.row_main_checkout"),
                            state,
                        ))
                    }),
            )
            .when(mode == MergeMode::Squash, |el| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .px(px(18.0))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(t!("integration.merge.row_message")),
                        )
                        .child(div().child(message.clone())),
                )
            })
            .child(
                div()
                    .mt(px(8.0))
                    .px(px(18.0))
                    .child(
                        div()
                            .id("integration-merge-diff-toggle")
                            .h(px(24.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .hover(|style| {
                                style.bg(crate::theme::wash(0.08)).text_color(theme.text)
                            })
                            .child(
                                icon(if diff_open {
                                    icons::ALT_ARROW_DOWN
                                } else {
                                    icons::ALT_ARROW_RIGHT
                                })
                                .size(px(11.0)),
                            )
                            .child(t!("integration.merge.final_diff"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(MergeState::Dialog { diff_open, .. }) =
                                    this.merge.as_mut().map(|flow| &mut flow.state)
                                {
                                    *diff_open = !*diff_open;
                                }
                                cx.notify();
                            })),
                    )
                    .when(diff_open, |el| {
                        el.child(
                            div()
                                .id("integration-merge-diff")
                                .mt(px(6.0))
                                .max_h(px(260.0))
                                .overflow_y_scroll()
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(theme.border.opacity(0.6))
                                .bg(theme.bg)
                                .p(px(10.0))
                                .font_family(theme.font_mono.clone())
                                .text_size(px(10.5))
                                .text_color(theme.text_muted)
                                .child(patch),
                        )
                    }),
            )
            .child(
                div()
                    .mt(px(14.0))
                    .mb(px(16.0))
                    .mx(px(18.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            theme,
                            &t!("integration.merge.cancel"),
                            "integration-merge-cancel",
                        )
                        .id("integration-merge-cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_merge(cx))),
                    )
                    .child(
                        popover::btn_primary(theme, &t!("integration.merge.confirm"))
                            .id("integration-merge-confirm")
                            .debug_selector(|| "integration-merge-confirm".into())
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_merge(cx))),
                    ),
            );
        popover::modal(
            "integration-merge-dialog",
            window.viewport_size(),
            card.into_any_element(),
        )
    }

    /// The conflict-resolution message dialog.
    fn render_resolve_dialog(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(dialog) = &self.resolve else {
            return div().into_any_element();
        };
        let input = dialog.input.clone();
        let card = popover::dialog_card(theme)
            .w(px(520.0))
            .id("integration-resolve-dialog-card")
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.resolve = None;
                cx.notify();
            }))
            .child(popover::dialog_title(
                theme,
                &t!("integration.merge.resolve_title"),
            ))
            .child(
                div()
                    .mt(px(10.0))
                    .px(px(18.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(t!("integration.merge.resolve_body")),
            )
            .child(div().mt(px(10.0)).mx(px(18.0)).child(input))
            .child(
                div()
                    .mt(px(14.0))
                    .mb(px(16.0))
                    .mx(px(18.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            theme,
                            &t!("integration.merge.cancel"),
                            "integration-resolve-cancel",
                        )
                        .id("integration-resolve-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.resolve = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        popover::btn_primary(theme, &t!("integration.merge.resolve_send"))
                            .id("integration-resolve-send")
                            .on_click(cx.listener(|this, _, _, cx| this.send_resolution(cx))),
                    ),
            );
        popover::modal(
            "integration-resolve-dialog",
            window.viewport_size(),
            card.into_any_element(),
        )
    }

    /// The worktree-delete confirmation.
    fn render_delete_dialog(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(confirm) = &self.delete_confirm else {
            return div().into_any_element();
        };
        let worktree = confirm.worktree.clone();
        let row = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .worktrees
                    .iter()
                    .find(|row| row.name == confirm.worktree)
            })
            .cloned();
        let branch = row
            .as_ref()
            .and_then(|row| row.branch.clone())
            .unwrap_or_default();
        let path = row.as_ref().map(|row| row.path.clone()).unwrap_or_default();
        let dirty = confirm.dirty;
        let card = popover::dialog_card(theme)
            .w(px(480.0))
            .id("integration-delete-dialog-card")
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.delete_confirm = None;
                cx.notify();
            }))
            .child(popover::dialog_title(
                theme,
                &t!("integration.delete.title"),
            ))
            .child(
                div()
                    .mt(px(10.0))
                    .px(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .child(kv_row(
                        theme,
                        &t!("integration.delete.row_worktree"),
                        worktree.clone(),
                    ))
                    .when(!branch.is_empty(), |el| {
                        el.child(kv_row(
                            theme,
                            &t!("integration.delete.row_branch"),
                            branch.clone(),
                        ))
                    })
                    .child(kv_row(
                        theme,
                        &t!("integration.delete.row_path"),
                        path.clone(),
                    ))
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .child(t!("integration.delete.chat_kept")),
                    )
                    .when(dirty, |el| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_color(theme.danger)
                                .child(t!("integration.delete.dirty_warning")),
                        )
                    }),
            )
            .child(
                div()
                    .mt(px(14.0))
                    .mb(px(16.0))
                    .mx(px(18.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            theme,
                            &t!("integration.delete.cancel"),
                            "integration-delete-cancel",
                        )
                        .id("integration-delete-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.delete_confirm = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        popover::btn_danger(theme, &t!("integration.delete.confirm"))
                            .id("integration-delete-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_delete(cx))),
                    ),
            );
        popover::modal(
            "integration-delete-dialog",
            window.viewport_size(),
            card.into_any_element(),
        )
    }

    /// A Commits-tab row click: open the commit's diff in the detail as a
    /// pinned pane over the worktree's checkout.
    fn open_commit_diff(&mut self, commit: holt_proto::GitHistoryCommit, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if let Some(snapshot) = &self.snapshot
            && let Some(worktree) = snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.name == detail.worktree)
        {
            let pin = detail.pin(worktree);
            let state = self.state.clone();
            let pane = cx.new(|cx| Changes::pinned(state, pin, Some(commit.clone()), None, cx));
            detail.commit = Some((commit, pane));
            cx.notify();
        }
    }
}

impl gpui::EventEmitter<IntegrationEvent> for IntegrationPage {}

impl Focusable for IntegrationPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for IntegrationPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        self.ensure_started(cx);
        let open = self.selected.is_some();
        let inbox = self.render_inbox(&theme, cx);
        let body = if open {
            div()
                .size_full()
                .flex()
                .min_h_0()
                .child(
                    div()
                        .id("integration-inbox-column")
                        .w(px(400.0))
                        .flex_none()
                        .min_h_0()
                        .border_r_1()
                        .border_color(theme.border.opacity(0.45))
                        .child(inbox),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .px(px(28.0))
                        .pt(px(20.0))
                        .pb(px(8.0))
                        .child(self.render_detail(&theme, cx)),
                )
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .max_w(px(INBOX_MAX_W))
                        .mx_auto()
                        .child(inbox),
                )
                .into_any_element()
        };

        div()
            .id("integration-page")
            .debug_selector(|| "integration-page".into())
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    crate::settings::widgets::error_strip(&theme, error)
                        .id("integration-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
            })
            .child(body)
            .child(self.render_merge_dialog(&theme, window, cx))
            .child(self.render_resolve_dialog(&theme, window, cx))
            .child(self.render_review_dialog(&theme, window, cx))
            .child(self.render_delete_dialog(&theme, window, cx))
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The primary merge button (the bar's right edge).
fn primary_merge_button(
    theme: &Theme,
    cx: &mut Context<IntegrationPage>,
) -> gpui::Stateful<gpui::Div> {
    popover::btn_primary(theme, &t!("integration.merge.merge_button"))
        .id("integration-merge-start")
        .debug_selector(|| "integration-merge-start".into())
        .on_click(cx.listener(|this, _, _, cx| {
            // A behind branch can only squash: coerce the mode rather than
            // trusting the toggle's disabled state.
            let mode = match (this.merge_mode, this.selected_row()) {
                (MergeMode::FastForward, Some(row)) if row.behind > 0 => MergeMode::Squash,
                (mode, _) => mode,
            };
            this.start_merge(mode, cx);
        }))
}

/// A disabled primary button (working branches still show the affordance).
fn disabled_primary(theme: &Theme, label: &str) -> AnyElement {
    let label = label.to_string();
    div()
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(8.0))
        .bg(theme.solid.opacity(0.35))
        .text_size(px(12.0))
        .text_color(theme.on_solid.opacity(0.5))
        .child(label)
        .into_any_element()
}

impl IntegrationPage {
    fn mode_chip(
        &mut self,
        mode: MergeMode,
        selected: MergeMode,
        disabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("integration-mode-{mode:?}")))
            .h(px(24.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .text_size(px(11.5))
            .when(disabled, |el| el.opacity(0.4))
            .when(!disabled, |el| el.cursor_pointer())
            .when(selected == mode, |el| el.text_color(theme.text))
            .when(selected != mode, |el| el.text_color(theme.text_muted))
            .when(!disabled && selected != mode, |el| {
                el.hover(|style| style.text_color(theme.text))
            })
            .when(!disabled, |el| {
                el.on_click(cx.listener(move |this, _, _, cx| {
                    this.merge_mode = mode;
                    cx.notify();
                }))
            })
            .child(mode.label().into_owned())
            .into_any_element()
    }
}

fn refusal_label(reason: holt_proto::MergeRefusal) -> std::borrow::Cow<'static, str> {
    match reason {
        holt_proto::MergeRefusal::NothingToMerge => {
            t!("integration.merge.refusal.nothing_to_merge")
        }
        holt_proto::MergeRefusal::FfNotPossible => t!("integration.merge.refusal.ff_not_possible"),
        holt_proto::MergeRefusal::Stale => t!("integration.merge.refusal.stale"),
        holt_proto::MergeRefusal::TrunkCheckedOutDirty => {
            t!("integration.merge.refusal.trunk_checked_out_dirty")
        }
        holt_proto::MergeRefusal::TrunkCheckedOutElsewhere => {
            t!("integration.merge.refusal.trunk_checked_out_elsewhere")
        }
        holt_proto::MergeRefusal::WorktreeMissing => {
            t!("integration.merge.refusal.worktree_missing")
        }
        holt_proto::MergeRefusal::BranchDetached => t!("integration.merge.refusal.branch_detached"),
    }
}

/// The confirm dialog's main-checkout row.
fn main_checkout_state(snapshot: &IntegrationSnapshot, theme: &Theme) -> Option<String> {
    let _ = theme;
    let checkout = &snapshot.main_checkout;
    Some(match (&checkout.branch, checkout.dirty) {
        (Some(branch), false) => t!(
            "integration.merge.main_on_trunk_clean",
            branch = branch.as_str()
        )
        .into_owned(),
        (Some(branch), true) => t!(
            "integration.merge.main_on_trunk_dirty",
            branch = branch.as_str()
        )
        .into_owned(),
        (None, _) => t!("integration.merge.main_elsewhere").into_owned(),
    })
}

fn kv_row(theme: &Theme, label: &str, value: String) -> AnyElement {
    let label = label.to_string();
    div()
        .flex()
        .gap(px(10.0))
        .child(
            div()
                .flex_none()
                .w(px(110.0))
                .text_color(theme.text_faint)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.text_muted)
                .child(value),
        )
        .into_any_element()
}

/// Join `ListReviews` rows with their chat rows' findings (the findings
/// live on the row `submit_review` wrote).
fn enrich_reviews(
    rows: Vec<serde_json::Value>,
    state: &Entity<AppState>,
    cx: &mut Context<IntegrationPage>,
) -> Vec<serde_json::Value> {
    let app = state.read(cx);
    rows.into_iter()
        .map(|mut row| {
            if let Some(chat_id) = row["chatId"].as_str()
                && let Some(chat) = app.chat_row(chat_id)
                && let Some(review) = &chat.review
            {
                row["findings"] = serde_json::to_value(review.findings.clone().unwrap_or_default())
                    .unwrap_or(serde_json::json!([]));
            }
            row
        })
        .collect()
}

/// The send-back message (§9.5): numbered findings with severity,
/// location, title, and detail.
fn send_back_body(reviewer: &str, findings: &[serde_json::Value]) -> String {
    let mut body = t!("integration.review.send_prefix", reviewer = reviewer).into_owned();
    body.push_str("\n\n");
    for (index, finding) in findings.iter().enumerate() {
        let severity = finding["severity"].as_str().unwrap_or("?");
        let path = finding["path"].as_str().unwrap_or("?");
        let at = finding["line"]
            .as_u64()
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        let title = finding["title"].as_str().unwrap_or("");
        body.push_str(&format!(
            "{}. [{severity}] {path}{at} — {title}\n",
            index + 1
        ));
        if let Some(detail) = finding["detail"].as_str().filter(|d| !d.is_empty()) {
            for line in detail.lines() {
                body.push_str("   ");
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    body
}

/// A history row's state label and its color.
fn review_state_label(review: &serde_json::Value, theme: &Theme) -> (SharedString, gpui::Hsla) {
    match (review["state"].as_str(), review["verdict"].as_str()) {
        (_, Some("clean")) => (t!("integration.review.clean").into(), theme.success),
        (_, Some("issues")) => (t!("integration.review.issues").into(), theme.danger),
        (Some("running"), _) => (t!("integration.review.running").into(), theme.busy),
        _ => (t!("integration.review.incomplete").into(), theme.text_faint),
    }
}

/// One check's live log view (the `SubscribeCheckLog` stream folded into
/// lines).
pub(crate) struct CheckLogView {
    lines: Vec<String>,
    _task: Option<Task<()>>,
}

impl gpui::Render for CheckLogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        div()
            .id("integration-check-log")
            .mx(px(12.0))
            .mb(px(10.0))
            .max_h(px(200.0))
            .overflow_y_scroll()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .bg(theme.bg)
            .p(px(10.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .children(self.lines.iter().cloned().map(|line| div().child(line)))
    }
}

/// One `TerminalEvent` as a display line.
fn check_log_line(event: &serde_json::Value) -> String {
    use base64::Engine as _;
    if let Some(data) = event.get("data").and_then(|value| value.as_str()) {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        return String::new();
    }
    if let Some(code) = event.get("exit").and_then(|value| value.as_i64()) {
        return format!("\n— exit {code} —");
    }
    if event.get("gap").is_some() {
        return "…".to_string();
    }
    String::new()
}

/// Milliseconds as a compact human duration.
fn format_duration_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// Whether deleting this worktree would take the trunk or the main
/// checkout's own branch with it — the engine refuses the deletion, so the
/// bar does not offer it.
pub(crate) fn delete_protected(
    row_branch: Option<&str>,
    trunk: &str,
    main_branch: Option<&str>,
) -> bool {
    row_branch.is_some_and(|branch| branch == trunk || Some(branch) == main_branch)
}

#[cfg(test)]
mod tests {
    use super::{
        Filter, RepoOption, WorktreeStatus, default_repo, filter_matches, repo_options,
        worktree_is_orphan, worktree_status,
    };
    use crate::state::AppState;
    use holt_proto::{Chat, IntegrationWorktree, Space};

    fn worktree(name: &str, chat_id: Option<&str>) -> IntegrationWorktree {
        IntegrationWorktree {
            name: name.to_string(),
            path: format!("/data/worktrees/{name}"),
            branch: Some(name.to_string()),
            head_sha: Some("a".repeat(40)),
            fork_sha: Some("b".repeat(40)),
            ahead: 1,
            behind: 0,
            dirty: false,
            dirty_files: 0,
            merged: false,
            merge_sha: None,
            missing: false,
            chat_id: chat_id.map(str::to_string),
            commits: Vec::new(),
            truncated: false,
        }
    }

    // ---- §6.4 status derivation ----

    #[test]
    fn status_takes_the_first_matching_rule_in_order() {
        // Missing wins over everything.
        let mut row = worktree("gone", None);
        row.missing = true;
        row.merged = true;
        row.dirty = true;
        assert_eq!(worktree_status(&row, true), WorktreeStatus::Missing);

        // Working requires a chat AND a live session.
        let row = worktree("busy", Some("chat-1"));
        assert_eq!(worktree_status(&row, false), WorktreeStatus::Ready);
        assert_eq!(worktree_status(&row, true), WorktreeStatus::Working);
        let orphan = worktree("lonely", None);
        assert_eq!(
            worktree_status(&orphan, true),
            WorktreeStatus::Ready,
            "no chat means no working state"
        );

        // Merged, then dirty, then no-changes, then behind.
        let mut row = worktree("m", Some("chat-1"));
        row.merged = true;
        assert_eq!(worktree_status(&row, true), WorktreeStatus::Working);
        assert_eq!(worktree_status(&row, false), WorktreeStatus::Merged);

        let mut row = worktree("d", None);
        row.dirty = true;
        row.ahead = 0;
        row.behind = 3;
        assert_eq!(worktree_status(&row, false), WorktreeStatus::Dirty);

        let mut row = worktree("n", None);
        row.ahead = 0;
        row.behind = 2;
        assert_eq!(worktree_status(&row, false), WorktreeStatus::NoChanges);

        let mut row = worktree("b", None);
        row.behind = 2;
        assert_eq!(worktree_status(&row, false), WorktreeStatus::Behind);

        assert_eq!(
            worktree_status(&worktree("r", None), false),
            WorktreeStatus::Ready
        );
    }

    #[test]
    fn orphan_marks_engine_worktrees_without_a_chat() {
        let data_dir = std::path::Path::new("/data");
        let stranded = worktree("stranded", None);
        assert!(worktree_is_orphan(&stranded, data_dir));
        // With a chat: not orphaned.
        let owned = worktree("owned", Some("chat-1"));
        assert!(!worktree_is_orphan(&owned, data_dir));
        // Outside the engine's worktrees dir: an imported worktree, not
        // orphaned.
        let imported = IntegrationWorktree {
            path: "/somewhere/else".to_string(),
            ..worktree("imported", None)
        };
        assert!(!worktree_is_orphan(&imported, data_dir));
    }

    #[test]
    fn filters_partition_the_statuses() {
        use WorktreeStatus::{Behind, Dirty, Merged, Missing, NoChanges, Ready, Working};
        assert!(filter_matches(Filter::All, Missing));
        assert!(filter_matches(Filter::Blocked, Missing));
        assert!(!filter_matches(Filter::Blocked, Ready));
        assert!(filter_matches(Filter::Active, Working));
        assert!(filter_matches(Filter::Active, Dirty));
        assert!(filter_matches(Filter::Active, NoChanges));
        assert!(!filter_matches(Filter::Active, Ready));
        assert!(filter_matches(Filter::Ready, Ready));
        assert!(filter_matches(Filter::Ready, Behind));
        assert!(!filter_matches(Filter::Ready, Dirty));
        assert!(filter_matches(Filter::Merged, Merged));
        assert!(!filter_matches(Filter::Merged, Ready));
    }

    #[test]
    fn send_back_body_lists_location_title_and_detail() {
        let body = super::send_back_body(
            "bugbot",
            &[serde_json::json!({
                "severity": "high",
                "path": "src/a.rs",
                "line": 3,
                "title": "unchecked index",
                "detail": "`v[i]` panics\nwhen empty",
            })],
        );
        assert!(
            body.contains("1. [high] src/a.rs:3 — unchecked index"),
            "{body}"
        );
        assert!(body.contains("   `v[i]` panics\n   when empty"), "{body}");
    }

    #[test]
    fn delete_is_hidden_when_the_branch_is_the_trunk_or_the_main_checkouts() {
        use super::delete_protected;
        // The trunk.
        assert!(delete_protected(Some("main"), "main", Some("side")));
        // The main checkout's own branch.
        assert!(delete_protected(Some("side"), "main", Some("side")));
        // An ordinary branch deletes.
        assert!(!delete_protected(Some("feature"), "main", Some("main")));
        // Detached or missing worktrees have nothing to protect.
        assert!(!delete_protected(None, "main", Some("main")));
    }

    // ---- repo selection ----

    fn space(id: &str, path: &str, git: bool) -> Space {
        Space {
            id: id.into(),
            device_id: "dev".into(),
            path: path.into(),
            name: None,
            git_detected: git,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn repo_options_skip_home_and_session_worktrees_and_dedupe_paths() {
        let spaces = vec![
            space(holt_proto::HOME_SPACE_ID, "/home-root", true),
            space("wt-chat-1", "/data/worktrees/chat-1", true),
            space("s2", "/repo-b", true),
            space("s1", "/repo-a", true),
            space("s1-dup", "/repo-a/", true), // same path, different row
            space("s3", "/not-git", false),
        ];
        let options = repo_options(&spaces);
        let paths: Vec<&str> = options.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, ["/repo-a", "/repo-b"]);
    }

    #[test]
    fn default_repo_follows_the_selected_chats_worktree_repo() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s1", "/repo-a", true),
            space("s2", "/repo-b", true),
        ]);
        let options: Vec<RepoOption> = repo_options(&state.spaces)
            .into_iter()
            .map(|o| RepoOption {
                label: o.label,
                path: o.path,
            })
            .collect();

        // No chat: the first option.
        assert_eq!(default_repo(&state, &options).as_deref(), Some("/repo-a"));

        // A session-worktree chat names the main checkout it forked from.
        let base_time = chrono::DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        let chat = Chat {
            id: "chat-1".into(),
            device_id: "dev".into(),
            title: None,
            title_source: holt_proto::TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: base_time,
            space_id: Some("wt-chat-1".into()),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            worktree: Some(holt_proto::WorktreeSpec {
                repo_path: "/repo-b".into(),
                base: "main".into(),
            }),
            provider_mode: false,
            routine_run: None,
            goal: None,
            review: None,
        };
        state.chats.push(chat);
        state.selected_chat = Some("chat-1".into());
        assert_eq!(default_repo(&state, &options).as_deref(), Some("/repo-b"));
    }
}
