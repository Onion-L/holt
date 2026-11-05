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
    focus: FocusHandle,
    focus_pending: bool,
    blink: Blink,
    task: Option<Task<()>>,
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
            focus: cx.focus_handle(),
            focus_pending: true,
            blink: Cell::new(false),
            task: None,
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
        detail.render(worktree, title, status, snapshot, &state, theme, cx)
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
