//! The far-right File tree: one `FileTreePanel` entity browsing the current
//! Chat's working directory (the selected Space's folder before a Chat
//! exists). Directories load one level per expansion through the engine's
//! `ListWorkspaceEntries`; expansion and selection are per Space (ADR-0020)
//! and survive Chat switches within the Space. Rows are virtualized through
//! gpui's `list` — expanding a huge directory adds one request, never a
//! recursive scan.

use std::collections::{HashMap, HashSet};

use gpui::prelude::*;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListState, SharedString, Task, WeakEntity, div, list, px,
};
use holt_proto::{WorkspaceEntryKind, WorkspaceListing};

use crate::icons::{self, icon};
use crate::loaders;
use crate::state::AppState;
use crate::theme::Theme;

use super::FileStateMap;

const ROW_HEIGHT: f32 = 26.0;

/// One directory's load state inside a Space's tree.
enum DirState {
    Loading,
    Loaded(WorkspaceListing),
    Failed(SharedString),
}

/// Per-Space browsing state — kept alive across Chat switches (ADR-0020).
struct SpaceTree {
    dirs: HashMap<String, DirState>,
    /// Request sequence per directory — a newer request for the same dir
    /// invalidates older replies.
    dir_requests: HashMap<String, u64>,
    expanded: HashSet<String>,
    selection: Option<String>,
}

impl SpaceTree {
    fn new() -> Self {
        Self {
            dirs: HashMap::new(),
            dir_requests: HashMap::new(),
            expanded: HashSet::new(),
            selection: None,
        }
    }
}

/// The root the panel is currently browsing, derived from the app state.
#[derive(Clone)]
struct ActiveRoot {
    /// The Space key file state is keyed by (ADR-0020).
    space_key: String,
    /// RPC selector — the selected Chat when one exists (the tree follows
    /// its working directory), the Space on the new-chat canvas.
    chat_id: Option<String>,
    space_id: Option<String>,
    /// Requests bind to this generation; a root/space switch invalidates
    /// every in-flight reply.
    generation: u64,
}

/// Events up to the shell: the tree never owns tabs, it just asks for opens.
pub enum FileTreeEvent {
    /// Coalesced disk changes under the active root (ticket 04) — the shell
    /// fans these out to the space's open viewers (clean ones reload; dirty
    /// ones enter the conflict state).
    DiskChanged { paths: Vec<String> },
    /// Open a file — `pin` marks the double-click path; single clicks ask
    /// for a replaceable preview. `resolved` carries the engine-resolved
    /// path for inside-root symlink aliases when the tree already knows it.
    OpenFile {
        path: String,
        resolved: Option<String>,
        pin: bool,
    },
}

/// A UI-only row that is not a workspace entry: the muted notices under an
/// expanded directory (truncated listings, empty folders).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowMarker {
    /// The listing hit the per-directory entry cap.
    Truncated,
    /// The directory loaded successfully and has no entries.
    Empty,
}

/// One flattened visible row (rebuilt whenever listings or expansion change).
#[derive(Clone)]
struct TreeRow {
    depth: usize,
    name: SharedString,
    path: String,
    kind: WorkspaceEntryKind,
    #[allow(dead_code)]
    marker: Option<RowMarker>,
}

pub struct FileTreePanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    list: ListState,
    /// The Space trees by key, retained for the session.
    spaces: HashMap<String, SpaceTree>,
    active: Option<ActiveRoot>,
    /// The visible rows for the active space (drives the ListState count).
    rows: Vec<TreeRow>,
    /// The live watch task for the active root (replaced on switch — the
    /// old stream drops, ending the engine-side watch with it).
    watch_task: Option<Task<()>>,
}

impl Focusable for FileTreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<FileTreeEvent> for FileTreePanel {}

/// A row is expandable when it is (or resolves to) a directory that stays
/// inside the root. Outside-root and broken links are dead-end rows with an
/// external-open affordance instead.
fn expandable(kind: &WorkspaceEntryKind) -> bool {
    match kind {
        WorkspaceEntryKind::Directory => true,
        WorkspaceEntryKind::SymlinkInside { target_is_dir, .. } => *target_is_dir,
        _ => false,
    }
}

/// The directory path expansion operates on: the symlink's resolved target
/// for aliases, the entry itself otherwise.
/// Depth-first flatten of the loaded + expanded tree into visible rows.
/// `ancestors` carries the canonical directory chain currently open above
/// `dir` — a symlink whose target is an ancestor (e.g. `ln -s . link`) is a
/// legal inside-root entry the engine happily reports, so recursion is only
/// pruned for a true ancestor cycle, never for a diamond alias.
fn flatten_into(
    space: &SpaceTree,
    dir: &str,
    depth: usize,
    ancestors: &mut Vec<String>,
    out: &mut Vec<TreeRow>,
) {
    let Some(DirState::Loaded(listing)) = space.dirs.get(dir) else {
        return;
    };
    if listing.entries.is_empty() {
        out.push(TreeRow {
            depth,
            name: SharedString::from("(empty)"),
            path: dir.to_string(),
            kind: WorkspaceEntryKind::Directory,
            marker: Some(RowMarker::Empty),
        });
        return;
    }
    for entry in &listing.entries {
        out.push(TreeRow {
            depth,
            name: entry.name.clone().into(),
            path: entry.path.clone(),
            kind: entry.kind.clone(),
            marker: None,
        });
        if expandable(&entry.kind) {
            let expansion = expansion_path(&entry.path, &entry.kind);
            if space.expanded.contains(&expansion)
                && !ancestors.iter().any(|ancestor| ancestor == &expansion)
            {
                ancestors.push(expansion.clone());
                flatten_into(space, &expansion, depth + 1, ancestors, out);
                ancestors.pop();
            }
        }
    }
    if listing.truncated {
        out.push(TreeRow {
            depth,
            name: SharedString::from("(listing truncated)"),
            path: format!("{dir}/…"),
            kind: WorkspaceEntryKind::File,
            marker: Some(RowMarker::Truncated),
        });
    }
}

fn expansion_path(path: &str, kind: &WorkspaceEntryKind) -> String {
    match kind {
        WorkspaceEntryKind::SymlinkInside { resolved_path, .. } => resolved_path.clone(),
        _ => path.to_string(),
    }
}

/// Open a path with the platform handler (outside-root links, unsupported
/// files). Percent-encodes so spaces and Unicode survive the URL parse.
pub(super) fn open_externally(path: &str, cx: &mut App) {
    let mut url = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    cx.open_url(&url);
}

impl FileTreePanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let list = ListState::new(0, gpui::ListAlignment::Top, px(200.0))
            .with_uniform_item_height(px(ROW_HEIGHT));
        let mut panel = Self {
            state,
            focus_handle,
            list,
            spaces: HashMap::new(),
            active: None,
            rows: Vec::new(),
            watch_task: None,
        };
        panel.sync_root(cx);
        // Re-derive the root whenever the selection moves — switching Chats
        // within a Space keeps the tree; switching Spaces swaps it whole.
        cx.observe(&panel.state, |this, _, cx| {
            this.sync_root(cx);
            cx.notify();
        })
        .detach();
        panel
    }

    /// The ActiveRoot the current selection implies, or None when there is
    /// nothing to browse (no Space, no Chat).
    fn derive_root(&self, cx: &App, generation: u64) -> Option<ActiveRoot> {
        let state = self.state.read(cx);
        let space_key = FileStateMap::space_key(state)?;
        if let Some(chat) = state.selected_chat_row() {
            Some(ActiveRoot {
                space_key,
                chat_id: Some(chat.id.clone()),
                space_id: None,
                generation,
            })
        } else {
            Some(ActiveRoot {
                space_key,
                chat_id: None,
                space_id: state.selected_space.clone(),
                generation,
            })
        }
    }

    /// Re-derive the active root; a Space switch resets the list state and
    /// invalidates in-flight work by generation. A Chat switch within the
    /// same Space only re-scopes the RPC selector.
    fn sync_root(&mut self, cx: &mut Context<Self>) {
        let switched = match (&self.active, self.derive_root(cx, 0)) {
            (Some(active), Some(derived)) => active.space_key != derived.space_key,
            (None, None) => false,
            _ => true,
        };
        let generation = match (&self.active, switched) {
            (Some(active), true) => active.generation + 1,
            _ => self
                .active
                .as_ref()
                .map(|root| root.generation)
                .unwrap_or(0),
        };
        let Some(derived) = self.derive_root(cx, generation) else {
            if self.active.is_some() {
                self.active = None;
                self.rows.clear();
                self.list.reset(0);
            }
            return;
        };
        self.spaces
            .entry(derived.space_key.clone())
            .or_insert_with(SpaceTree::new);
        self.active = Some(derived);
        if switched {
            self.list.reset(0);
            self.rebuild_rows();
            self.ensure_dir_loaded("", cx);
            self.start_watch(cx);
        }
    }

    /// One watch per active root. Frames re-list the affected directories
    /// (keeping expansion and selection) and tell the shell what moved so
    /// open viewers can react. A root switch drops the subscription — the
    /// engine-side watch ends with it.
    fn start_watch(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(active) = self.active.clone() else {
            return;
        };
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &active.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &active.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        let params = serde_json::Value::Object(params);
        let generation = active.generation;
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            let Ok(mut frames) = engine
                .client()
                .subscribe(holt_rpc::methods::WATCH_WORKSPACE_ENTRIES, params)
                .await
            else {
                return;
            };
            while let Some(frame) = frames.recv().await {
                let Ok(frame) = serde_json::from_value::<holt_proto::WorkspaceWatchFrame>(frame)
                else {
                    continue;
                };
                let _ = this.update(cx, |this, cx| {
                    let current = this.active.clone();
                    if current.is_none_or(|root| root.generation != generation) {
                        return; // stale after a switch
                    }
                    let reload = this.invalidate_changed_dirs(&frame.paths);
                    this.rebuild_rows();
                    for dir in reload {
                        this.ensure_dir_loaded(&dir, cx);
                    }
                    cx.emit(FileTreeEvent::DiskChanged {
                        paths: frame.paths.clone(),
                    });
                    cx.notify();
                });
            }
        }));
    }

    /// Drop the cached listings for the directories that changed (and for
    /// changed directories themselves when expanded), then reload whichever
    /// are still shown so the tree reflects current disk state. The ROOT's
    /// listing is keyed `""` — the canonical root path maps onto it.
    fn invalidate_changed_dirs(&mut self, paths: &[String]) -> Vec<String> {
        let Some(active) = self.active.clone() else {
            return Vec::new();
        };
        let root_canonical = self
            .spaces
            .get(&active.space_key)
            .and_then(|space| match space.dirs.get("") {
                Some(DirState::Loaded(listing)) => Some(listing.path.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut stale: Vec<String> = Vec::new();
        {
            let Some(space) = self.spaces.get_mut(&active.space_key) else {
                return Vec::new();
            };
            for path in paths {
                let changed = std::path::Path::new(path);
                let parent = changed
                    .parent()
                    .map(|parent| parent.display().to_string())
                    .unwrap_or_default();
                let parent_key = if parent == root_canonical {
                    String::new()
                } else {
                    parent
                };
                stale.push(parent_key);
                // A changed directory's own listing is stale when expanded.
                stale.push(path.clone());
            }
            for dir in &stale {
                if space.dirs.remove(dir).is_some() {
                    space.dir_requests.remove(dir);
                }
            }
        }
        // Reload: the root always (if it was loaded), expanded dirs, and any
        // stale dir whose parent is still expanded (visible rows).
        let mut reload: Vec<String> = Vec::new();
        for dir in &stale {
            let shown = dir.is_empty() || {
                self.spaces
                    .get(&active.space_key)
                    .map(|space| space.expanded.contains(dir))
                    .unwrap_or(false)
            };
            if shown {
                reload.push(dir.clone());
            }
        }
        reload
    }

    /// The `ListWorkspaceEntries` params for the current root.
    fn selector_params(&self, path: &str) -> serde_json::Value {
        let active = self
            .active
            .as_ref()
            .expect("callers check for an active root");
        let mut params = serde_json::Map::new();
        if let Some(chat_id) = &active.chat_id {
            params.insert("chatId".into(), serde_json::json!(chat_id));
        } else if let Some(space_id) = &active.space_id {
            params.insert("spaceId".into(), serde_json::json!(space_id));
        }
        if !path.is_empty() {
            params.insert("path".into(), serde_json::json!(path));
        }
        serde_json::Value::Object(params)
    }

    /// Read a value off the active Space's tree.
    fn with_active_space<R>(&self, f: impl FnOnce(&SpaceTree) -> R) -> Option<R> {
        let active = self.active.as_ref()?;
        self.spaces.get(&active.space_key).map(f)
    }

    /// Load one directory (empty string = the root) unless it already holds
    /// state. Stale guards: root generation + per-dir request sequence.
    fn ensure_dir_loaded(&mut self, dir: &str, cx: &mut Context<Self>) {
        let Some(active) = self.active.clone() else {
            return;
        };
        // Engine availability first: inserting `Loading` with no way to
        // answer it would strand the row forever (sync_root does not re-fire
        // for the same space key).
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let space = self
            .spaces
            .entry(active.space_key.clone())
            .or_insert_with(SpaceTree::new);
        if space.dirs.contains_key(dir) {
            return;
        }
        space.dirs.insert(dir.to_string(), DirState::Loading);
        let request = space.dir_requests.entry(dir.to_string()).or_insert(0);
        *request += 1;
        let request = *request;
        let params = self.selector_params(dir);
        let generation = active.generation;
        let space_key = active.space_key;
        let dir = dir.to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                holt_rpc::methods::LIST_WORKSPACE_ENTRIES,
                params,
                std::time::Duration::from_secs(15),
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                // Obsolete after a root switch or a newer request for the
                // same directory — the reply is dropped, not applied.
                let current = this.active.as_ref()?;
                if current.generation != generation || current.space_key != space_key {
                    return None;
                }
                let space = this
                    .spaces
                    .entry(space_key.clone())
                    .or_insert_with(SpaceTree::new);
                if space.dir_requests.get(&dir).copied() != Some(request) {
                    return None;
                }
                let state = match reply {
                    Ok(value) => match serde_json::from_value::<WorkspaceListing>(value) {
                        Ok(listing) => DirState::Loaded(listing),
                        Err(error) => DirState::Failed(format!("unexpected reply: {error}").into()),
                    },
                    Err(message) => DirState::Failed(message.into()),
                };
                space.dirs.insert(dir.clone(), state);
                this.rebuild_rows();
                cx.notify();
                None::<()>
            });
        })
        .detach();
    }

    /// Rebuild the flattened visible rows for the active Space. Splice (not
    /// reset) so expanding a row keeps the scroll offset.
    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(active) = self.active.clone() else {
            return;
        };
        let Some(space) = self.spaces.get(&active.space_key) else {
            return;
        };
        let mut rows = Vec::new();
        let mut ancestors = Vec::new();
        flatten_into(space, "", 0, &mut ancestors, &mut rows);
        let count = rows.len();
        self.rows = rows;
        let previous = self.list.item_count();
        if previous != count {
            self.list.splice(0..previous, count);
        }
    }

    fn row_index_for_path(&self, path: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.path == path)
    }

    fn selection_path(&self) -> Option<String> {
        self.with_active_space(|space| space.selection.clone())
            .flatten()
    }

    fn selection_index(&self) -> Option<usize> {
        let path = self.selection_path()?;
        self.row_index_for_path(&path)
    }

    /// Select a row and keep it visible.
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.rows.get(ix).map(|row| row.path.clone()) else {
            return;
        };
        let Some(active) = self.active.clone() else {
            return;
        };
        self.spaces
            .entry(active.space_key)
            .or_insert_with(SpaceTree::new)
            .selection = Some(path);
        self.list.scroll_to_reveal_item(ix);
        cx.notify();
    }

    /// Toggle a directory's expansion, loading its children on expand.
    fn toggle_expansion(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        if !expandable(&row.kind) {
            return;
        }
        let Some(active) = self.active.clone() else {
            return;
        };
        let expansion = expansion_path(&row.path, &row.kind);
        let was_expanded = self
            .spaces
            .entry(active.space_key.clone())
            .or_insert_with(SpaceTree::new)
            .expanded
            .remove(&expansion);
        if was_expanded {
            // Collapse keeps the listing cached for a cheap re-expand.
            self.rebuild_rows();
            cx.notify();
        } else {
            self.spaces
                .get_mut(&active.space_key)
                .expect("just inserted")
                .expanded
                .insert(expansion.clone());
            self.ensure_dir_loaded(&expansion, cx);
            self.rebuild_rows();
            cx.notify();
        }
    }

    /// Activate a row: files open (preview or pinned), directories toggle.
    fn activate_row(&mut self, ix: usize, pin: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        self.select(ix, cx);
        match &row.kind {
            WorkspaceEntryKind::File => {
                cx.emit(FileTreeEvent::OpenFile {
                    path: row.path,
                    resolved: None,
                    pin,
                });
            }
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir: false,
                resolved_path,
            } => {
                let resolved = resolved_path.clone();
                cx.emit(FileTreeEvent::OpenFile {
                    path: row.path,
                    resolved: Some(resolved),
                    pin,
                });
            }
            WorkspaceEntryKind::Directory
            | WorkspaceEntryKind::SymlinkInside {
                target_is_dir: true,
                ..
            } => {
                self.toggle_expansion(ix, cx);
            }
            // Outside-root and broken links never open inside the sidebar;
            // the row's external-open affordance is the deliberate path.
            WorkspaceEntryKind::SymlinkOutside { .. } | WorkspaceEntryKind::SymlinkBroken => {}
        }
    }

    /// Retry a failed directory load.
    fn retry_dir(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        let expansion = expansion_path(&row.path, &row.kind);
        let Some(active) = self.active.clone() else {
            return;
        };
        self.spaces
            .entry(active.space_key)
            .or_insert_with(SpaceTree::new)
            .dirs
            .remove(&expansion);
        self.ensure_dir_loaded(&expansion, cx);
        self.rebuild_rows();
        cx.notify();
    }

    fn handle_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let rows = self.rows.len();
        let current = self.selection_index().unwrap_or(0);
        match key {
            "down" if rows > 0 => {
                self.select((current + 1).min(rows - 1), cx);
            }
            "up" if rows > 0 => {
                self.select(current.saturating_sub(1), cx);
            }
            "right" => {
                let Some(row) = self.rows.get(current).cloned() else {
                    return;
                };
                let expanded = self
                    .with_active_space(|space| {
                        space
                            .expanded
                            .contains(&expansion_path(&row.path, &row.kind))
                    })
                    .unwrap_or(false);
                if expandable(&row.kind) && !expanded {
                    self.toggle_expansion(current, cx);
                } else if rows > 0 {
                    self.select((current + 1).min(rows - 1), cx);
                }
            }
            "left" => {
                let Some(row) = self.rows.get(current).cloned() else {
                    return;
                };
                let expanded = self
                    .with_active_space(|space| {
                        space
                            .expanded
                            .contains(&expansion_path(&row.path, &row.kind))
                    })
                    .unwrap_or(false);
                if expandable(&row.kind) && expanded {
                    self.toggle_expansion(current, cx);
                } else {
                    // Step out to the parent directory's row.
                    let parent = row
                        .path
                        .trim_end_matches('/')
                        .rsplit_once('/')
                        .map(|(parent, _)| parent.to_string());
                    if let Some(parent) = parent
                        && let Some(ix) = self.row_index_for_path(&parent)
                    {
                        self.select(ix, cx);
                    }
                }
            }
            "enter" | "space" => {
                self.activate_row(current, false, cx);
            }
            _ => {}
        }
    }

    /// The panel body: empty state without a Space, a loading/error state for
    /// the root, and the virtualized rows otherwise.
    pub(crate) fn render_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let body: AnyElement = if self.active.is_some() {
            let root_state = self
                .with_active_space(|space| match space.dirs.get("") {
                    Some(DirState::Loading) => Some(None),
                    Some(DirState::Failed(message)) => Some(Some(message.clone())),
                    _ => None,
                })
                .flatten();
            match root_state {
                Some(None) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child("Loading…"),
                    )
                    .into_any_element(),
                Some(Some(message)) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(16.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.danger_muted)
                            .child(message),
                    )
                    .into_any_element(),
                None => list(self.list.clone(), cx.processor(Self::render_row))
                    .size_full()
                    .into_any_element(),
            }
        } else {
            // No Space selected: an explicit empty state, not an error.
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
                            icon(icons::FOLDER_WITH_FILES)
                                .size(px(18.0))
                                .text_color(theme.text_muted.opacity(0.6)),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child("Select a space to browse its files"),
                        ),
                )
                .into_any_element()
        };
        div()
            .id("file-tree")
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            // Clicking anywhere in the tree lands keyboard focus so its
            // navigation keys (arrows/enter) go to the tree, not the composer.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                cx.stop_propagation();
                this.handle_key(event, cx);
            }))
            .child(body)
            .into_any_element()
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let selected = self.selection_path().as_deref() == Some(row.path.as_str());
        let expand = expandable(&row.kind);
        let expanded = self
            .with_active_space(|space| {
                space
                    .expanded
                    .contains(&expansion_path(&row.path, &row.kind))
            })
            .unwrap_or(false);
        let loading = self
            .with_active_space(|space| {
                matches!(
                    space.dirs.get(&expansion_path(&row.path, &row.kind)),
                    Some(DirState::Loading)
                )
            })
            .unwrap_or(false);
        let failed = self
            .with_active_space(|space| {
                matches!(
                    space.dirs.get(&expansion_path(&row.path, &row.kind)),
                    Some(DirState::Failed(_))
                )
            })
            .unwrap_or(false);

        let group: SharedString = format!("file-tree-row-{ix}").into();
        let indent = px(6.0) + px(row.depth as f32 * 13.0);

        let chevron: AnyElement = if loading {
            loaders::mini_glyph_spinner(
                format!("file-tree-dir-{ix}"),
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        } else if expand {
            icon(if expanded {
                icons::ALT_ARROW_DOWN
            } else {
                icons::ALT_ARROW_RIGHT
            })
            .size(px(12.0))
            .flex_none()
            .text_color(theme.text_muted.opacity(0.7))
            .into_any_element()
        } else {
            div().size(px(12.0)).flex_none().into_any_element()
        };

        let kind_icon = match &row.kind {
            WorkspaceEntryKind::Directory => icons::FOLDER,
            WorkspaceEntryKind::File => icons::DOCUMENT,
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir: true,
                ..
            } => icons::FOLDER,
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir: false,
                ..
            } => icons::DOCUMENT,
            WorkspaceEntryKind::SymlinkOutside { .. } | WorkspaceEntryKind::SymlinkBroken => {
                icons::ARROW_UP_RIGHT
            }
        };

        let mut row_el = div()
            .id(("file-tree-row", ix))
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(indent)
            .pr(px(6.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .group(group.clone())
            .when(selected, |el| el.bg(crate::theme::wash(0.08)))
            .when(!selected, |el| {
                el.hover(|state| state.bg(crate::theme::wash(0.05)))
            })
            .child(
                div()
                    .id(("file-tree-disclosure", ix))
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(expand, |el| {
                        el.hover(|state| state.bg(crate::theme::wash(0.10)).rounded(px(3.0)))
                    })
                    .when(expand, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.toggle_expansion(ix, cx);
                        }))
                    })
                    .child(chevron),
            )
            .child(
                icon(kind_icon)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.8)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text.opacity(0.85)
                    })
                    .child(row.name.clone()),
            )
            .tooltip({
                let path = row.path.clone();
                move |_, cx| {
                    cx.new(|_| crate::image_viewer::ViewerTooltip(path.clone().into()))
                        .into()
                }
            });

        // Outside-root and broken links get an explicit external-open action
        // (decision 15) — they are never opened inside the sidebar.
        match &row.kind {
            WorkspaceEntryKind::SymlinkOutside { .. } | WorkspaceEntryKind::SymlinkBroken => {
                let path = row.path.clone();
                row_el = row_el.child(
                    div()
                        .id(("file-tree-external", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.0)
                        .group_hover(group, |state| state.opacity(1.0))
                        .hover(|state| state.bg(crate::theme::wash(0.10)))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            open_externally(&path, cx);
                        }))
                        .child(
                            icon(icons::ARROW_UP_RIGHT)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                );
            }
            _ if failed => {
                let message = self
                    .with_active_space(|space| {
                        match space.dirs.get(&expansion_path(&row.path, &row.kind)) {
                            Some(DirState::Failed(message)) => message.clone(),
                            _ => SharedString::from("failed to load"),
                        }
                    })
                    .unwrap_or_else(|| SharedString::from("failed to load"));
                row_el = row_el.child(
                    div()
                        .id(("file-tree-retry", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.0)
                        .group_hover(group, |state| state.opacity(1.0))
                        .hover(|state| state.bg(crate::theme::wash(0.10)))
                        .tooltip(move |_, cx| {
                            cx.new(|_| {
                                crate::image_viewer::ViewerTooltip(
                                    format!("Retry: {message}").into(),
                                )
                            })
                            .into()
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.retry_dir(ix, cx);
                        }))
                        .child(
                            icon(icons::REFRESH)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                );
            }
            _ => {}
        }

        row_el
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                // Files: single click previews, double-click pins (decision
                // 12). Directories toggle on either.
                let pin = event.click_count() >= 2;
                this.activate_row(ix, pin, cx);
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::WorkspaceEntry;

    fn entry(name: &str, path: &str, kind: WorkspaceEntryKind) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.into(),
            path: path.into(),
            kind,
            size: None,
        }
    }

    fn alias_to_root(root: &str) -> WorkspaceEntry {
        entry(
            "link",
            &format!("{root}/link"),
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir: true,
                resolved_path: root.to_string(),
            },
        )
    }

    /// `ln -s . link` inside the root: a legal inside-root alias the engine
    /// reports as expandable. Flattening must terminate instead of recursing
    /// through the ancestor chain forever.
    #[test]
    fn ancestor_symlink_expansion_terminates() {
        let root = "/tmp/space-1";
        let mut space = SpaceTree::new();
        space.dirs.insert(
            "".to_string(),
            DirState::Loaded(WorkspaceListing {
                path: root.to_string(),
                entries: vec![
                    alias_to_root(root),
                    entry(
                        "file.txt",
                        &format!("{root}/file.txt"),
                        WorkspaceEntryKind::File,
                    ),
                ],
                truncated: false,
            }),
        );
        // The alias expands to the root's own canonical path; its listing
        // (loaded under that key) contains the alias again.
        space.dirs.insert(
            root.to_string(),
            DirState::Loaded(WorkspaceListing {
                path: root.to_string(),
                entries: vec![alias_to_root(root)],
                truncated: false,
            }),
        );
        space.expanded.insert(root.to_string());
        let mut rows = Vec::new();
        let mut ancestors = Vec::new();
        flatten_into(&space, "", 0, &mut ancestors, &mut rows);
        // Root's two rows + the alias's single non-recursed level — bounded.
        assert!(
            rows.len() <= 4,
            "ancestor alias must not recurse unboundedly ({} rows)",
            rows.len()
        );
    }

    #[test]
    fn truncated_and_empty_listings_get_marker_rows() {
        let mut space = SpaceTree::new();
        space.dirs.insert(
            "/r".to_string(),
            DirState::Loaded(WorkspaceListing {
                path: "/r".into(),
                entries: vec![entry("a", "/r/a", WorkspaceEntryKind::File)],
                truncated: true,
            }),
        );
        space.expanded.insert("/r".to_string());
        let mut rows = Vec::new();
        let mut ancestors = Vec::new();
        flatten_into(&space, "/r", 0, &mut ancestors, &mut rows);
        assert!(
            rows.iter()
                .any(|row| row.marker == Some(RowMarker::Truncated))
        );

        let mut empty = SpaceTree::new();
        empty.dirs.insert(
            "/e".to_string(),
            DirState::Loaded(WorkspaceListing {
                path: "/e".into(),
                entries: vec![],
                truncated: false,
            }),
        );
        let mut rows = Vec::new();
        let mut ancestors = Vec::new();
        flatten_into(&empty, "/e", 0, &mut ancestors, &mut rows);
        assert!(rows.iter().any(|row| row.marker == Some(RowMarker::Empty)));
    }
}
