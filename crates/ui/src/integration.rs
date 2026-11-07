//! The integration page (glossary: integration view): one repository's
//! trunk and every linked worktree — the branch tree, per-worktree detail,
//! commits, and the branch diff. Read-only; merging arrives with its own
//! slice. Data comes from `ListIntegration` plus the chats/sessions watches
//! the shell already holds.

pub(crate) mod detail;
mod graph;

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, PathBuilder, Render, SharedString,
    Subscription, Task, Window, canvas, div, point, prelude::*, px,
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
use graph::{LANE_WIDTH, ROW_HEIGHT, RowKind, TRUNK_X, TreeLayout, graph_width};

/// Left list width bounds (clamp(340, 46%, 560) in the prototype).
const LIST_MIN_W: f32 = 340.0;
const LIST_MAX_W: f32 = 560.0;
const LIST_FRACTION: f32 = 0.46;
/// The working dot's blink cadence.
const BLINK_TICK: Duration = Duration::from_millis(650);

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

/// The list filter. Non-matching rows stay visible, dimmed — hiding them
/// would tear the tree apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Active,
    Ready,
    Blocked,
    Merged,
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

/// The blink phase for working heads, toggled by the ticker.
type Blink = Cell<bool>;

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
    resolve: Option<ResolveDialog>,
    delete_confirm: Option<DeleteConfirm>,
    deleting: Option<String>,
    focus: FocusHandle,
    focus_pending: bool,
    blink: Blink,
    task: Option<Task<()>>,
    /// The merge flow's own slot: a snapshot reload or a delete must never
    /// cancel an in-flight preview/confirm — a dropped task would strand
    /// `MergeState::Previewing` with nothing to settle it.
    merge_task: Option<Task<()>>,
    delete_task: Option<Task<()>>,
    refs_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    _observe: Subscription,
    _blink: Option<Task<()>>,
}

impl IntegrationPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
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
            resolve: None,
            delete_confirm: None,
            deleting: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            blink: Cell::new(false),
            task: None,
            merge_task: None,
            delete_task: None,
            refs_task: None,
            fetch_task: None,
            _observe: observe,
            _blink: None,
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
        // The session-status re-derivation needs no fetch; the render reads
        // the sessions watch live.
        self.ensure_blink(cx);
        cx.notify();
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
            return;
        }
        self.selected = Some(name.to_string());
        let trunk = self.trunk_of().unwrap_or_default();
        self.detail = Detail::rebuild(self.detail.take(), name, trunk);
        self.merge = None;
        self.resolve = None;
        self.delete_confirm = None;
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
        self.task = Some(cx.spawn(async move |this, cx| {
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
                        // Keep the selection when it survived the reload.
                        if page
                            .selected
                            .as_ref()
                            .is_none_or(|name| !snapshot.worktrees.iter().any(|w| &w.name == name))
                        {
                            page.selected = snapshot.worktrees.first().map(|w| w.name.clone());
                        }
                        // The detail pane follows the selection and the
                        // trunk; a stable key keeps its tab and folds, a
                        // moved one rebuilds.
                        let selected = page.selected.clone().unwrap_or_default();
                        let trunk_now = page
                            .trunk
                            .clone()
                            .unwrap_or_else(|| snapshot.trunk.name.clone());
                        let key_matches = page.detail.as_ref().is_some_and(|detail| {
                            detail.worktree == selected && detail.trunk == trunk_now
                        });
                        if !key_matches {
                            page.detail = Detail::rebuild(page.detail.take(), &selected, trunk_now);
                        } else if let Some(detail) = &page.detail {
                            // Same worktree, maybe a new head: refetch the
                            // embedded diff panes.
                            detail.invalidate(cx);
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

    /// Blink the working heads: a ticker only while something works and the
    /// user allows motion.
    fn ensure_blink(&mut self, cx: &mut Context<Self>) {
        if self._blink.is_some() {
            return;
        }
        self._blink = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_TICK).await;
                let alive = this.update(cx, |page, cx| {
                    if page.any_working(cx) && !crate::motion::reduced_motion(cx) {
                        page.blink.set(!page.blink.get());
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn any_working(&self, cx: &App) -> bool {
        let Some(snapshot) = &self.snapshot else {
            return false;
        };
        let state = self.state.read(cx);
        snapshot.worktrees.iter().any(|worktree| {
            worktree
                .chat_id
                .as_deref()
                .and_then(|id| state.session_for(id))
                .is_some_and(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Working | SessionStatus::Compacting
                    )
                })
        })
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
        let repo_menu_open = self.repo_menu.is_open();
        let trunk_menu_open = self.trunk_menu.is_open();

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .pb(px(14.0))
            .child(
                div()
                    .text_size(px(19.0))
                    .text_color(theme.text)
                    .child(t!("integration.title")),
            )
            // Repo picker: one dropdown of the project spaces.
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
            .child(div().flex_1())
            .child(self.render_filters(theme, cx))
            .child(self.render_refresh(theme, cx))
            .when(repo_menu_open, |el| {
                let closing = self.repo_menu.closing_since();
                el.child(popover::anchored_menu_below(
                    "integration-repo-menu",
                    self.render_repo_menu(&options, theme, cx),
                    closing,
                ))
            })
            .when(trunk_menu_open, |el| {
                let closing = self.trunk_menu.closing_since();
                el.child(popover::anchored_menu_below(
                    "integration-trunk-menu",
                    self.render_trunk_menu(theme, cx),
                    closing,
                ))
            })
            .into_any_element()
    }

    fn render_repo_trigger(
        &mut self,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
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
            .bg(crate::theme::wash(0.05))
            .hover(|style| style.bg(crate::theme::wash(0.12)))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, window, _| {
                    window.prevent_default();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.repo_menu.take_press_was_open() || this.repo_menu.is_closing() {
                    this.repo_menu.begin_close();
                } else {
                    this.trunk_menu.begin_close();
                    this.repo_menu.open(());
                }
                cx.notify();
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
            )
    }

    fn render_repo_menu(
        &mut self,
        options: &[RepoOption],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                            this.repo_menu.begin_close();
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
            .children(rows)
            .into_any_element()
    }

    fn render_trunk_trigger(
        &mut self,
        trunk: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
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
                cx.listener(|_, _, window, _| {
                    window.prevent_default();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.trunk_menu.take_press_was_open() || this.trunk_menu.is_closing() {
                    this.trunk_menu.begin_close();
                } else {
                    this.repo_menu.begin_close();
                    this.trunk_menu.open(());
                }
                cx.notify();
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
            )
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
                            this.trunk_menu.begin_close();
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
            .children(rows)
            .into_any_element()
    }

    fn filter_counts(&self, cx: &App) -> [usize; 5] {
        let statuses = self.statuses(cx);
        let mut counts = [0usize; 5];
        for (status, _) in statuses.values() {
            for (index, filter) in [
                Filter::All,
                Filter::Active,
                Filter::Ready,
                Filter::Blocked,
                Filter::Merged,
            ]
            .into_iter()
            .enumerate()
            {
                if filter_matches(filter, *status) {
                    counts[index] += 1;
                }
            }
        }
        counts
    }

    fn render_filters(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use Filter::{Active, All, Blocked, Merged, Ready};
        let filter = self.filter;
        let counts = self.filter_counts(cx);
        let entries: [(Filter, SharedString, usize); 5] = [
            (All, t!("integration.filter.all").into(), counts[0]),
            (Active, t!("integration.filter.active").into(), counts[1]),
            (Ready, t!("integration.filter.ready").into(), counts[2]),
            (Blocked, t!("integration.filter.blocked").into(), counts[3]),
            (Merged, t!("integration.filter.merged").into(), counts[4]),
        ];
        div()
            .id("integration-filters")
            .flex()
            .items_center()
            .p(px(3.0))
            .rounded(px(10.0))
            .bg(crate::theme::wash(0.06))
            .children(entries.into_iter().map(|(entry, label, count)| {
                let active = entry == filter;
                div()
                    .id(SharedString::from(format!("integration-filter-{entry:?}")))
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .when(active, |el| {
                        el.bg(theme.surface_raised)
                            .shadow_sm()
                            .text_color(theme.text)
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
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(count.to_string())),
                    )
            }))
            .into_any_element()
    }

    fn render_refresh(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let fetching = self.fetching;
        div()
            .id("integration-refresh")
            .h(px(26.0))
            .px(px(10.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(theme.text_muted)
            .hover(|style| style.bg(crate::theme::wash(0.1)).text_color(theme.text))
            .when(fetching, |el| el.text_color(theme.text_faint))
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            .child(
                icon(icons::REFRESH)
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.75)),
            )
            .child(if fetching {
                t!("integration.refreshing").into_any_element()
            } else {
                t!("integration.refresh").into_any_element()
            })
            .into_any_element()
    }

    /// The left column: legend, the branch tree canvas behind, and the rows
    /// on top of it (rows carry the clicks; the canvas never does).
    fn render_list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            let label = if self.loading {
                t!("integration.loading")
            } else if self.repo_path.is_none() {
                t!("integration.repo_none")
            } else {
                t!("integration.empty")
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.0))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .text_center()
                .child(label)
                .into_any_element();
        };
        let layout = self
            .layout
            .as_ref()
            .cloned()
            .unwrap_or_else(|| graph::layout(&snapshot));
        let statuses = self.statuses(cx);
        let selected = self.selected.clone();
        let filter = self.filter;
        let graph_width = graph::graph_width(layout.lane_count);
        let blink = self.blink.get();
        let trunk_name = snapshot.trunk.name.clone();
        let trunk_head = snapshot.trunk.head_sha.clone();
        let now = Utc::now();

        let unmerged = snapshot
            .worktrees
            .iter()
            .filter(|worktree| !worktree.merged)
            .count();

        let rows: Vec<AnyElement> = layout
            .rows
            .iter()
            .enumerate()
            .map(|(row_index, kind)| match kind {
                RowKind::Trunk(commit_index) => {
                    let Some(commit) = snapshot.trunk_commits.get(*commit_index) else {
                        return gpui::Empty.into_any_element();
                    };
                    let is_head = commit.sha == trunk_head;
                    div()
                        .pl(px(graph_width))
                        .pr(px(12.0))
                        .h(px(ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .flex_none()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .when(is_head, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .mr(px(7.0))
                                    .px(px(5.0))
                                    .rounded(px(5.0))
                                    .bg(theme.text_muted)
                                    .text_size(px(9.5))
                                    .font_family(theme.font_mono.clone())
                                    .text_color(theme.surface)
                                    .child(trunk_name.clone()),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .w(px(46.0))
                                .font_family(theme.font_mono.clone())
                                .text_size(px(10.5))
                                .child(short_sha(&commit.sha)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(11.5))
                                .child(commit.subject.clone()),
                        )
                        .into_any_element()
                }
                RowKind::Worktree(worktree_index) => {
                    let Some(worktree) = snapshot.worktrees.get(*worktree_index) else {
                        return gpui::Empty.into_any_element();
                    };
                    self.render_worktree_row(
                        worktree,
                        row_index,
                        &statuses,
                        selected.as_deref(),
                        filter,
                        &trunk_name,
                        graph_width,
                        now,
                        theme,
                        cx,
                    )
                }
            })
            .collect();

        let canvas = self.render_graph_canvas(
            &layout,
            &snapshot,
            &statuses,
            selected.clone(),
            blink,
            theme,
        );

        div()
            .id("integration-list")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .relative()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.0))
                            .pl(px(graph_width))
                            .h(px(ROW_HEIGHT))
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.text_faint)
                            .child(match snapshot.worktrees.len() {
                                0 => t!("integration.legend.worktrees.zero"),
                                1 => t!("integration.legend.worktrees.one"),
                                n => t!("integration.legend.worktrees.other", n = n),
                            })
                            .child(match unmerged {
                                0 => t!("integration.legend.unmerged.zero"),
                                1 => t!("integration.legend.unmerged.one"),
                                n => t!("integration.legend.unmerged.other", n = n),
                            }),
                    )
                    .child(canvas)
                    .children(rows),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_worktree_row(
        &mut self,
        worktree: &holt_proto::IntegrationWorktree,
        _row_index: usize,
        statuses: &std::collections::HashMap<String, (WorktreeStatus, bool)>,
        selected: Option<&str>,
        filter: Filter,
        trunk: &str,
        graph_width: f32,
        now: chrono::DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (status, orphan) = statuses
            .get(&worktree.name)
            .copied()
            .unwrap_or((WorktreeStatus::Missing, false));
        let is_selected = selected == Some(worktree.name.as_str());
        let dimmed = !filter_matches(filter, status);
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
        let age = worktree
            .commits
            .first()
            .and_then(|commit| chrono::DateTime::parse_from_rfc3339(&commit.authored_at).ok())
            .map(|time| crate::state::format_time_ago(time.with_timezone(&Utc), now))
            .unwrap_or_default();
        let ahead_behind: SharedString = if worktree.merged {
            t!(
                "integration.row.merged_as",
                sha = short_sha(worktree.merge_sha.as_deref().unwrap_or(""))
            )
            .into()
        } else {
            let mut text = format!("↑{}", worktree.ahead);
            if worktree.behind > 0 {
                text.push_str(&format!(" ↓{}", worktree.behind));
            }
            text.into()
        };
        div()
            .id(SharedString::from(format!(
                "integration-worktree-row-{}",
                worktree.name
            )))
            .debug_selector({
                let name = worktree.name.clone();
                move || format!("integration-worktree-row-{name}")
            })
            .pl(px(graph_width))
            .pr(px(12.0))
            .h(px(ROW_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(10.0))
            .cursor_pointer()
            .when(dimmed, |el| el.opacity(0.35))
            .when(is_selected, |el| el.bg(crate::theme::wash(0.07)))
            .when(!is_selected, |el| {
                el.hover(|style| style.bg(crate::theme::wash(0.04)))
            })
            .on_click({
                let name = worktree.name.clone();
                cx.listener(move |this, _, _, cx| this.select_worktree(&name, cx))
            })
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .text_color(if is_selected {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .child(title),
                            )
                            .when(orphan, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .px(px(4.0))
                                        .rounded(px(4.0))
                                        .bg(crate::theme::wash(0.08))
                                        .text_size(px(9.0))
                                        .text_color(theme.text_faint)
                                        .child(t!("integration.orphan")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .max_w(px(160.0))
                                    .font_family(theme.font_mono.clone())
                                    .child(worktree.branch.clone().unwrap_or_default()),
                            )
                            .child(div().flex_none().child(ahead_behind))
                            .when(!age.is_empty(), |el| el.child(div().flex_none().child(age))),
                    ),
            )
            .child(Self::status_chip(status, trunk, theme))
            .into_any_element()
    }

    /// The branch tree itself: trunk line, branch lanes, dots. Painted in
    /// one canvas behind the rows; row coordinates are index × ROW_HEIGHT
    /// plus the list's scroll handled by the containing scroll view.
    fn render_graph_canvas(
        &self,
        layout: &TreeLayout,
        snapshot: &IntegrationSnapshot,
        statuses: &std::collections::HashMap<String, (WorktreeStatus, bool)>,
        selected: Option<String>,
        blink: bool,
        theme: &Theme,
    ) -> AnyElement {
        let spans = layout.spans.clone();
        let rows = layout.rows.clone();
        let trunk_rows: Vec<(usize, String)> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                RowKind::Trunk(commit_index) => snapshot
                    .trunk_commits
                    .get(*commit_index)
                    .map(|commit| (index, commit.sha.clone())),
                _ => None,
            })
            .collect();
        let trunk_head = snapshot.trunk.head_sha.clone();
        let statuses = statuses.clone();
        let row_count = rows.len();
        let theme = theme.clone();
        canvas(
            move |_, _, _| {},
            move |bounds, _, window, _| {
                let y =
                    |row: usize| bounds.origin.y + px(row as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0);
                let lane_x = |lane: usize| bounds.origin.x + px(TRUNK_X + lane as f32 * LANE_WIDTH);
                let half = ROW_HEIGHT / 2.0;

                // Trunk spine.
                if let Some((first, _)) = trunk_rows.first() {
                    let mut trunk = PathBuilder::stroke(px(1.75));
                    trunk.move_to(point(lane_x(0), y(*first)));
                    trunk.line_to(point(lane_x(0), y(row_count.saturating_sub(1)) + px(half)));
                    if let Ok(path) = trunk.build() {
                        window.paint_path(path, theme.text_muted.opacity(0.55));
                    }
                }

                // Branch lanes: dashed when merged, accent when selected.
                for span in &spans {
                    let is_selected = selected.as_deref() == Some(span.name.as_str());
                    let color = if is_selected {
                        theme.accent
                    } else if span.dashed {
                        theme.border
                    } else {
                        theme.text_muted.opacity(0.75)
                    };
                    let mut builder = PathBuilder::stroke(px(if is_selected { 1.75 } else { 1.5 }));
                    if span.dashed {
                        builder = builder.dash_array(&[px(3.0), px(4.0)]);
                    }
                    let x = lane_x(span.lane);
                    let top = y(span.top_row);
                    let tip = y(span.tip_row);
                    let fork = y(span.fork_row);
                    if span.merge_back {
                        builder.move_to(point(lane_x(0), top));
                        builder.cubic_bezier_to(
                            point(lane_x(0), top + px(half)),
                            point(x, top + px(half)),
                            point(x, top + px(ROW_HEIGHT)),
                        );
                        builder.line_to(point(x, fork - px(ROW_HEIGHT)));
                    } else {
                        builder.move_to(point(x, tip));
                        builder.line_to(point(x, fork - px(ROW_HEIGHT)));
                    }
                    builder.cubic_bezier_to(
                        point(x, fork - px(half)),
                        point(lane_x(0), fork - px(half)),
                        point(lane_x(0), fork),
                    );
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, color);
                    }
                }

                // Trunk dots.
                for (index, sha) in &trunk_rows {
                    let head = *sha == trunk_head;
                    let mut dot = PathBuilder::fill();
                    let radius = if head { 4.5 } else { 3.25 };
                    circle(&mut dot, point(lane_x(0), y(*index)), px(radius));
                    if let Ok(path) = dot.build() {
                        window.paint_path(
                            path,
                            if head {
                                theme.text_muted
                            } else {
                                theme.text_faint.opacity(0.7)
                            },
                        );
                    }
                }

                // Branch head dots: working heads blink hollow.
                for span in &spans {
                    let is_selected = selected.as_deref() == Some(span.name.as_str());
                    let status = statuses
                        .get(&span.name)
                        .map(|(status, _)| *status)
                        .unwrap_or(WorktreeStatus::Ready);
                    let center = point(lane_x(span.lane), y(span.tip_row));
                    match status {
                        WorktreeStatus::Working => {
                            let color = if blink {
                                theme.busy.opacity(0.25)
                            } else {
                                theme.busy
                            };
                            let mut ring = PathBuilder::stroke(px(1.5));
                            circle(&mut ring, center, px(4.0));
                            if let Ok(path) = ring.build() {
                                window.paint_path(path, color);
                            }
                        }
                        _ => {
                            let mut dot = PathBuilder::fill();
                            circle(&mut dot, center, px(if is_selected { 5.0 } else { 4.25 }));
                            if let Ok(path) = dot.build() {
                                window.paint_path(
                                    path,
                                    if is_selected {
                                        theme.accent
                                    } else if status == WorktreeStatus::Merged {
                                        theme.text_faint.opacity(0.6)
                                    } else {
                                        theme.text_muted
                                    },
                                );
                            }
                        }
                    }
                }
            },
        )
        .absolute()
        .top(px(ROW_HEIGHT)) // below the legend line
        .left_0()
        .w(px(graph_width(layout.lane_count)))
        .h(px(row_count as f32 * ROW_HEIGHT + ROW_HEIGHT))
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
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(t!("integration.detail.none"))
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
        let Some(detail) = self.detail.as_mut() else {
            return div().size_full().into_any_element();
        };
        let pane = detail.render(worktree, title, status, snapshot, &state, theme, cx);
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
        self.merge_task = Some(cx.spawn(async move |this, cx| {
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
        self.merge_task = Some(cx.spawn(async move |this, cx| {
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
        self.merge_task = Some(cx.spawn(async move |this, cx| {
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
        let trunk = self.trunk_of();
        self.delete_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_WORKTREE,
                    serde_json::json!({
                        "repoPath": repo_path,
                        "worktree": confirm.worktree.clone(),
                        "force": confirm.dirty,
                        "trunk": trunk,
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
            snapshot.default_trunk.as_deref(),
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
                        .child(self.render_mode_toggle(&row, &trunk, theme, cx))
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
                    .child(self.render_mode_toggle(&row, &trunk, theme, cx))
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
        trunk: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.merge_mode;
        let ff_disabled = row.behind > 0;
        div()
            .id("integration-merge-modes")
            .flex()
            .items_center()
            .p(px(3.0))
            .rounded(px(10.0))
            .bg(crate::theme::wash(0.06))
            .child(self.mode_chip(MergeMode::Squash, selected, false, theme, cx))
            .child(self.mode_chip(MergeMode::FastForward, selected, ff_disabled, theme, cx))
            .when(ff_disabled, |el| {
                let hint = t!("integration.merge.ff_blocked", trunk = trunk).into_owned();
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
        let list_width =
            (LIST_FRACTION * f32::from(window.viewport_size().width)).clamp(LIST_MIN_W, LIST_MAX_W);
        let header = self.render_header(&theme, cx);
        let list = self.render_list(&theme, cx);
        let detail = self.render_detail(&theme, cx);

        div()
            .id("integration-page")
            .debug_selector(|| "integration-page".into())
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .pt(px(10.0))
            .px(px(22.0))
            .pb(px(6.0))
            .gap(px(14.0))
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
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(16.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(list_width))
                            .min_h_0()
                            .overflow_hidden()
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.border.opacity(0.6))
                            .child(list),
                    )
                    .child(div().flex_1().min_w_0().min_h_0().child(detail)),
            )
            .child(self.render_merge_dialog(&theme, window, cx))
            .child(self.render_resolve_dialog(&theme, window, cx))
            .child(self.render_delete_dialog(&theme, window, cx))
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Append a full circle to a fill builder (PathBuilder has no `circle`):
/// two half arcs from the leftmost point.
fn circle(
    builder: &mut gpui::PathBuilder,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
) {
    let left = point(center.x - radius, center.y);
    let right = point(center.x + radius, center.y);
    let radii = point(radius, radius);
    builder.move_to(left);
    builder.arc_to(radii, px(0.0), false, true, right);
    builder.arc_to(radii, px(0.0), false, true, left);
    builder.close();
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
            .when(selected == mode, |el| {
                el.bg(theme.surface_raised)
                    .shadow_sm()
                    .text_color(theme.text)
            })
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
        // A checkout on another branch never syncs: the engine moves the
        // trunk ref and leaves this working tree alone.
        (Some(branch), _) if branch != &snapshot.trunk.name => t!(
            "integration.merge.main_on_other",
            branch = branch.as_str(),
            trunk = snapshot.trunk.name.as_str()
        )
        .into_owned(),
        (Some(branch), true) => t!(
            "integration.merge.main_on_trunk_dirty",
            branch = branch.as_str()
        )
        .into_owned(),
        (Some(branch), false) => t!(
            "integration.merge.main_on_trunk_clean",
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

/// Whether deleting this worktree would take the trunk or the main
/// checkout's own branch with it — the engine refuses the deletion, so the
/// bar does not offer it.
pub(crate) fn delete_protected(
    row_branch: Option<&str>,
    trunk: &str,
    default_trunk: Option<&str>,
    main_branch: Option<&str>,
) -> bool {
    // Mirror of the engine's refusal: a worktree holding the selected
    // trunk, the repository's own default branch, or the main checkout's
    // branch never offers the delete.
    row_branch.is_some_and(|branch| {
        branch == trunk || Some(branch) == default_trunk || Some(branch) == main_branch
    })
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
    fn delete_is_hidden_when_the_branch_is_the_trunk_or_the_main_checkouts() {
        use super::delete_protected;
        // The trunk.
        assert!(delete_protected(
            Some("main"),
            "main",
            Some("side"),
            Some("side")
        ));
        // The repository's own default branch, even when the page merged
        // into a different trunk.
        assert!(delete_protected(
            Some("main"),
            "develop",
            Some("main"),
            Some("side")
        ));
        // The main checkout's own branch.
        assert!(delete_protected(
            Some("side"),
            "main",
            Some("main"),
            Some("side")
        ));
        // An ordinary branch deletes.
        assert!(!delete_protected(
            Some("feature"),
            "main",
            Some("main"),
            Some("main")
        ));
        // Detached or missing worktrees have nothing to protect.
        assert!(!delete_protected(None, "main", Some("main"), Some("main")));
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
        };
        state.chats.push(chat);
        state.selected_chat = Some("chat-1".into());
        assert_eq!(default_repo(&state, &options).as_deref(), Some("/repo-b"));
    }
}
