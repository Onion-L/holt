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
use holt_proto::{
    WorkspaceEntryKind, WorkspaceGitStatus, WorkspaceGitStatusKind, WorkspaceListing,
};

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

/// The prebuilt lookup for one working-tree Git status snapshot (ticket
/// 10): the canonical workdir the tree's row paths strip down to repo-
/// relative keys, plus the normalized path → kind map. `workdir: None`
/// (a non-Git Space) or a carried `error` means decorations off — never
/// tree failure.
#[derive(Clone)]
struct GitStatusIndex {
    workdir: Option<String>,
    error: Option<String>,
    map: HashMap<String, WorkspaceGitStatusKind>,
}

impl From<&WorkspaceGitStatus> for GitStatusIndex {
    fn from(snapshot: &WorkspaceGitStatus) -> Self {
        Self {
            workdir: snapshot.workdir.clone(),
            error: snapshot.error.clone(),
            map: snapshot
                .entries
                .iter()
                .map(|entry| (entry.path.clone(), entry.kind))
                .collect(),
        }
    }
}

/// A row's Git decoration (ticket 10): the exact repo-relative match, else
/// the nearest untracked-or-ignored ancestor directory — a wholly
/// untracked/ignored directory classifies every descendant. `None` is a
/// clean entry (or no repository): no marker, no subdued styling. A failed
/// snapshot never decorates — its empty entries are a git failure, not a
/// clean tree.
fn git_kind_for(index: &GitStatusIndex, path: &str) -> Option<WorkspaceGitStatusKind> {
    if index.error.is_some() {
        return None;
    }
    let workdir = index.workdir.as_deref()?;
    let rel = path.strip_prefix(workdir)?.trim_start_matches('/');
    if let Some(kind) = index.map.get(rel) {
        return Some(*kind);
    }
    let mut ancestor = rel;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        ancestor = parent;
        match index.map.get(ancestor) {
            Some(kind @ WorkspaceGitStatusKind::Ignored)
            | Some(kind @ WorkspaceGitStatusKind::Untracked) => return Some(*kind),
            _ => {}
        }
    }
    None
}

/// The marker letter and its tooltip text. `Ignored` renders no letter —
/// subdued styling is its whole treatment. Shared with the Git panel's
/// kind badges.
pub(crate) fn marker_parts(kind: WorkspaceGitStatusKind) -> Option<(&'static str, SharedString)> {
    use WorkspaceGitStatusKind as Kind;
    match kind {
        Kind::Untracked => Some((
            "U",
            SharedString::from("Untracked — new file, not yet in git"),
        )),
        Kind::Added => Some(("A", SharedString::from("Added — staged new file"))),
        Kind::Modified => Some(("M", SharedString::from("Modified — uncommitted changes"))),
        Kind::Conflicted => Some((
            "C",
            SharedString::from("Conflicted — unresolved merge conflict"),
        )),
        Kind::Deleted => Some(("D", SharedString::from("Deleted from the working tree"))),
        Kind::Ignored => None,
    }
}

/// The marker's color: new content green, changes and conflicts amber,
/// removals red. `Ignored` never carries a letter (see [`marker_parts`]);
/// the arm keeps the match exhaustive. Shared with the Git panel.
pub(crate) fn marker_color(kind: WorkspaceGitStatusKind, theme: &Theme) -> gpui::Hsla {
    use WorkspaceGitStatusKind as Kind;
    match kind {
        Kind::Untracked | Kind::Added => theme.success,
        Kind::Modified | Kind::Conflicted => theme.warning,
        Kind::Deleted => theme.danger,
        Kind::Ignored => theme.text_muted,
    }
}

/// Per-Space browsing state — kept alive across Chat switches (ADR-0020).
struct SpaceTree {
    dirs: HashMap<String, DirState>,
    /// Request sequence per directory — a newer request for the same dir
    /// invalidates older replies.
    dir_requests: HashMap<String, u64>,
    expanded: HashSet<String>,
    selection: Option<String>,
    /// The Space's pending-cut entry (ticket 07): its row renders dimmed
    /// until the entry is pasted, re-cut, or gone. Visual only — the
    /// authoritative cut state lives on the shell.
    cut: Option<String>,
    /// Latest working-tree Git status (ticket 10): None until the first
    /// frame lands; a non-Git root stores a `workdir: None` index.
    git_status: Option<GitStatusIndex>,
}

impl SpaceTree {
    fn new() -> Self {
        Self {
            dirs: HashMap::new(),
            dir_requests: HashMap::new(),
            expanded: HashSet::new(),
            selection: None,
            cut: None,
            git_status: None,
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

/// A context-menu request (ticket 06): where it opened (window position),
/// the directory new entries land in, and the row a Rename targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMenuTarget {
    pub position: gpui::Point<gpui::Pixels>,
    /// The parent for New file / New directory ("" = the root).
    pub parent: String,
    /// The entry a Rename acts on: (absolute path, current name).
    pub rename: Option<(String, String)>,
    /// The entry an "Add to Chat" attaches (ticket 09): inside-root entries
    /// only — outside-root and broken links stay external-open rows.
    pub attach: Option<String>,
}

/// A dragged tree entry (ticket 09): the ABSOLUTE entry path, bound at drag
/// start, so a drop into the composer attaches exactly what the row showed
/// even if the selection moves mid-gesture. Outside-root and broken symlinks
/// never start a drag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntryDrag {
    pub path: String,
    pub is_dir: bool,
    pub title: SharedString,
}

/// Ghost chip following the pointer while a tree entry drags.
struct TreeEntryGhost {
    title: SharedString,
    is_dir: bool,
}

impl gpui::Render for TreeEntryGhost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .w(px(140.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                icon(if self.is_dir {
                    icons::FOLDER
                } else {
                    icons::DOCUMENT
                })
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Events up to the shell: the tree never owns tabs, it just asks for opens.
pub enum FileTreeEvent {
    /// Coalesced disk changes under the active root (ticket 04) — the shell
    /// fans these out to the space's open viewers (clean ones reload; dirty
    /// ones enter the conflict state).
    DiskChanged { paths: Vec<String> },
    /// A context menu opened (right-click): the shell owns the menu and the
    /// create/rename dialogs it leads to (ticket 06).
    ContextMenu { target: FileMenuTarget },
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
    /// A UI-only notice row (truncated listing, empty folder) — never a
    /// drag source or a context-menu target.
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
    /// A reveal in flight (ticket 09): the absolute path being walked into
    /// the tree. Each ancestor expansion loads one level; the walk resumes
    /// as listings land and finishes by selecting the target's row.
    pending_reveal: Option<String>,
    /// The live watch task for the active root (replaced on switch — the
    /// old stream drops, ending the engine-side watch with it).
    watch_task: Option<Task<()>>,
    /// The working-tree Git status stream for the active root (ticket 10),
    /// replaced on switch the same way.
    status_task: Option<Task<()>>,
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
            pending_reveal: None,
            watch_task: None,
            status_task: None,
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
            self.restore_expansion(cx);
            self.list.reset(0);
            self.rebuild_rows();
            self.ensure_dir_loaded("", cx);
            self.start_watch(cx);
            self.start_status_watch(cx);
        }
    }

    /// Seed the switched-to Space's expansion from the persisted navigation
    /// record (ticket 05) — the directories the user left open reload their
    /// one level each; nothing else expands and the root never recurses.
    fn restore_expansion(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.clone() else {
            return;
        };
        // Only real Space identities persist; `cwd:` fallback keys stay
        // session-only. The KEY carries that (a chat-selected root still
        // belongs to its Space).
        if active.space_key.starts_with("cwd:") {
            return;
        }
        let record = crate::settings::current(cx)
            .file_navigation
            .get(&active.space_key)
            .cloned()
            .unwrap_or_default();
        if record.expanded.is_empty() {
            return;
        }
        let Some(space) = self.spaces.get_mut(&active.space_key) else {
            return;
        };
        for dir in record.expanded {
            space.expanded.insert(dir);
        }
        // Render rows for the seeded expansion, then load each expanded
        // directory's single level.
        self.rebuild_rows();
        let expanded: Vec<String> = self
            .spaces
            .get(&active.space_key)
            .map(|space| space.expanded.iter().cloned().collect())
            .unwrap_or_default();
        for dir in expanded {
            self.ensure_dir_loaded(&dir, cx);
        }
    }

    /// Direct refresh for one directory (ticket 06: successful mutations
    /// update the tree without waiting for the watch). "" = the root; the
    /// root's canonical spelling maps onto that key.
    pub(crate) fn refresh_dir(&mut self, dir: &str, cx: &mut Context<Self>) {
        let Some(active) = self.active.clone() else {
            return;
        };
        let dir = self
            .spaces
            .get(&active.space_key)
            .and_then(|space| match space.dirs.get("") {
                Some(DirState::Loaded(listing)) => Some(listing.path.clone()),
                _ => None,
            })
            .filter(|root| root == dir)
            .map(|_| String::new())
            .unwrap_or_else(|| dir.to_string());
        let shown = dir.is_empty()
            || self
                .spaces
                .get(&active.space_key)
                .map(|space| space.expanded.contains(&dir))
                .unwrap_or(false);
        if let Some(space) = self.spaces.get_mut(&active.space_key) {
            space.dirs.remove(&dir);
            space.dir_requests.remove(&dir);
        }
        if shown {
            self.ensure_dir_loaded(&dir, cx);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// A renamed/moved directory keeps its tree state under the new name
    /// (tickets 06 + 07): every expanded key under `from` rewrites to `to`,
    /// and the selection rides the same rewrite — a moved or renamed row
    /// stays selected.
    pub(crate) fn carry_tree_over_rename(&mut self, from: &str, to: &str) {
        let Some(active) = self.active.clone() else {
            return;
        };
        let Some(space) = self.spaces.get_mut(&active.space_key) else {
            return;
        };
        let prefix = format!("{}/", from.trim_end_matches('/'));
        let rewritten: Vec<String> = space
            .expanded
            .iter()
            .filter_map(|entry| {
                entry
                    .strip_prefix(&prefix)
                    .map(|rest| format!("{}/{}", to.trim_end_matches('/'), rest))
            })
            .collect();
        space.expanded.retain(|entry| !entry.starts_with(&prefix));
        // The renamed directory's own listing key follows it.
        if let Some(listing) = space.dirs.remove(from) {
            space.dir_requests.remove(from);
            space.dirs.insert(to.to_string(), listing);
        }
        space.expanded.insert(to.to_string());
        for entry in rewritten {
            space.expanded.insert(entry);
        }
        // The selection rides along when it pointed at the entry or a
        // descendant — same prefix rewrite as the expansion keys.
        if let Some(selection) = space.selection.clone() {
            let carried = if selection == from {
                Some(to.to_string())
            } else {
                selection
                    .strip_prefix(&prefix)
                    .map(|rest| format!("{}/{}", to.trim_end_matches('/'), rest))
            };
            if let Some(carried) = carried {
                space.selection = Some(carried);
            }
        }
    }

    /// Reveal a path in the tree (ticket 09): expand its ancestor chain one
    /// level at a time, then select the target row. The walk is resumable —
    /// each ancestor's listing lands asynchronously and re-runs
    /// [`Self::advance_reveal`]; a path that left the tree (or never existed)
    /// walks as deep as it can and stops, never blocking anything else.
    pub(crate) fn reveal_path(&mut self, path: &str, cx: &mut Context<Self>) {
        self.pending_reveal = Some(path.trim_end_matches('/').to_string());
        self.advance_reveal(cx);
    }

    /// The reveal currently walking, if any (read-only view for tests).
    #[cfg(test)]
    pub(crate) fn pending_reveal_path(&self) -> Option<&str> {
        self.pending_reveal.as_deref()
    }

    /// One step of the pending reveal: every ancestor whose listing is
    /// already loaded expands; the first missing one loads (the reply
    /// re-runs this); once the target's parent is expanded the target row
    /// is selected. A missing ancestor listing, a failed one, or a target
    /// outside the current root clears the reveal.
    fn advance_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pending_reveal.clone() else {
            return;
        };
        let Some(active) = self.active.clone() else {
            self.pending_reveal = None;
            return;
        };
        // The canonical root spells the ancestor prefixes; before it loads
        // the walk cannot even start.
        let root =
            match self
                .spaces
                .get(&active.space_key)
                .and_then(|space| match space.dirs.get("") {
                    Some(DirState::Loaded(listing)) => Some(listing.path.clone()),
                    _ => None,
                }) {
                Some(root) => root.trim_end_matches('/').to_string(),
                None => {
                    self.ensure_dir_loaded("", cx);
                    return;
                }
            };
        let Some(relative) = target
            .strip_prefix(&root)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|rest| !rest.is_empty())
        else {
            // The root itself or outside it — nothing to walk to.
            self.pending_reveal = None;
            return;
        };
        // All segments but the last name directories to expand; the last is
        // the row to select (file or folder — a folder reveals without
        // expanding, the tree's selection IS the reveal).
        #[derive(PartialEq)]
        enum Ancestor {
            Ready,
            Loading,
            Missing,
            Failed,
        }
        let segments: Vec<&str> = relative.split('/').collect();
        let mut prefix = root;
        for segment in &segments[..segments.len().saturating_sub(1)] {
            prefix = format!("{prefix}/{segment}");
            let state = self
                .spaces
                .get(&active.space_key)
                .map(|space| match space.dirs.get(&prefix) {
                    Some(DirState::Loaded(_)) => Ancestor::Ready,
                    Some(DirState::Loading) => Ancestor::Loading,
                    Some(DirState::Failed(_)) => Ancestor::Failed,
                    None => Ancestor::Missing,
                })
                .unwrap_or(Ancestor::Missing);
            match state {
                Ancestor::Ready | Ancestor::Missing => {
                    // Expand it (a missing ancestor loads; the reply resumes
                    // the walk).
                    if let Some(space) = self.spaces.get_mut(&active.space_key) {
                        space.expanded.insert(prefix.clone());
                    }
                    if state == Ancestor::Missing {
                        self.ensure_dir_loaded(&prefix, cx);
                        return;
                    }
                }
                // Already on its way — the reply resumes the walk.
                Ancestor::Loading => return,
                Ancestor::Failed => {
                    // The ancestor is gone or unreadable — the walk stops
                    // here, and the dead expansion flag goes with it.
                    if let Some(space) = self.spaces.get_mut(&active.space_key) {
                        space.expanded.remove(&prefix);
                    }
                    self.pending_reveal = None;
                    return;
                }
            }
        }
        self.rebuild_rows();
        match self.row_index_for_path(&target) {
            Some(ix) => {
                self.select(ix, cx);
                self.pending_reveal = None;
            }
            None => {
                // The parent loaded but the target is not in it — the entry
                // left since the search saw it. The reveal is done.
                self.pending_reveal = None;
                cx.notify();
            }
        }
    }

    /// Set (or clear) a Space's pending-cut marker — the shell owns the
    /// cut state; this only drives the dimmed-row rendering.
    pub(crate) fn set_cut(&mut self, space_key: &str, path: Option<&str>) {
        if let Some(space) = self.spaces.get_mut(space_key) {
            space.cut = path.map(str::to_string);
        }
    }

    /// Drop every Space's cut marker (one cut exists at a time; the shell
    /// clears before setting the next).
    pub(crate) fn clear_all_cuts(&mut self) {
        for space in self.spaces.values_mut() {
            space.cut = None;
        }
    }

    /// The expanded directory paths recorded for one Space (persistence
    /// reads this back; spaces never visited this run return None so their
    /// stored record stands).
    pub(crate) fn expanded_for(&self, space: &str) -> Option<Vec<String>> {
        self.spaces
            .get(space)
            .map(|tree| tree.expanded.iter().cloned().collect())
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

    /// One Git status stream per active root (ticket 10). Frames refresh
    /// the OWNING Space's decoration index — keyed by generation, so a
    /// reply that lands after a switch is dropped, not applied to the new
    /// Space. The rows read the index at render; no per-row Git work.
    fn start_status_watch(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(active) = self.active.clone() else {
            return;
        };
        let params = self.selector_params("");
        let generation = active.generation;
        let space_key = active.space_key;
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let Ok(mut frames) = engine
                .client()
                .subscribe(holt_rpc::methods::WATCH_WORKSPACE_GIT_STATUS, params)
                .await
            else {
                return;
            };
            while let Some(frame) = frames.recv().await {
                let Ok(snapshot) = serde_json::from_value::<WorkspaceGitStatus>(frame) else {
                    continue;
                };
                let _ = this.update(cx, |this, cx| {
                    this.apply_status_frame(generation, &space_key, snapshot);
                    cx.notify();
                });
            }
        }));
    }

    /// Store one status frame on its owning Space — unless the panel moved
    /// on (a Space switch bumped the generation) and the frame is stale.
    /// A stale frame is dropped, never applied to the new Space.
    fn apply_status_frame(
        &mut self,
        generation: u64,
        space_key: &str,
        snapshot: WorkspaceGitStatus,
    ) {
        let Some(current) = self.active.clone() else {
            return;
        };
        if current.generation != generation || current.space_key != space_key {
            return;
        }
        if let Some(space) = self.spaces.get_mut(space_key) {
            space.git_status = Some(GitStatusIndex::from(&snapshot));
        }
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
                // A pending reveal resumes as each ancestor's listing lands.
                if this.pending_reveal.is_some() {
                    this.advance_reveal(cx);
                }
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
            // Collapse keeps the listing cached for a cheap re-expand, and
            // drops the descendants' flags with it — they are invisible now
            // and must not silently reseed (or waste a listing) on restore.
            let prefix = format!("{expansion}/");
            if let Some(space) = self.spaces.get_mut(&active.space_key) {
                space.expanded.retain(|entry| !entry.starts_with(&prefix));
            }
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
            // Right-click on empty tree space offers creates at the root.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|_this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(FileTreeEvent::ContextMenu {
                        target: FileMenuTarget {
                            position: event.position,
                            parent: String::new(),
                            rename: None,
                            attach: None,
                        },
                    });
                }),
            )
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
        // The Space's pending-cut entry renders dimmed until pasted or
        // re-cut (ticket 07) — the visible half of the cut state.
        let is_cut = self
            .with_active_space(|space| space.cut.as_deref() == Some(row.path.as_str()))
            .unwrap_or(false);
        // Working-tree Git decoration (ticket 10): a marker letter for
        // changed entries, subdued styling for ignored ones. Read from the
        // Space's latest status index — never a Git call per row.
        let git_kind = self
            .with_active_space(|space| {
                space
                    .git_status
                    .as_ref()
                    .and_then(|index| git_kind_for(index, &row.path))
            })
            .flatten();
        let ignored = git_kind == Some(WorkspaceGitStatusKind::Ignored);
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
        // Ticket 09: plain entries and inside-root aliases attach to the
        // chat (context menu + drag into the composer); outside-root and
        // broken links stay external-open rows, and UI notice rows
        // (empty/truncated markers) never do.
        let attachable = row.marker.is_none()
            && !matches!(
                &row.kind,
                WorkspaceEntryKind::SymlinkOutside { .. } | WorkspaceEntryKind::SymlinkBroken
            );
        let is_dir = match &row.kind {
            WorkspaceEntryKind::Directory => true,
            WorkspaceEntryKind::SymlinkInside { target_is_dir, .. } => *target_is_dir,
            _ => false,
        };

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
            .when(is_cut, |el| el.opacity(0.55))
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
                    .text_color(if ignored {
                        theme.text_muted.opacity(0.45)
                    } else {
                        theme.text_muted.opacity(0.8)
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(if selected {
                        theme.text
                    } else if ignored {
                        // Subdued, but readable enough to browse a large
                        // ignored directory; selection keeps full contrast.
                        theme.text_muted.opacity(0.6)
                    } else {
                        theme.text.opacity(0.85)
                    })
                    .child(row.name.clone()),
            )
            // The status marker (ticket 10): one fixed-width letter at the
            // row's trailing edge — names never reflow when it appears.
            .when_some(
                git_kind.and_then(|kind| marker_parts(kind).map(|parts| (kind, parts))),
                |el, (kind, (letter, label))| {
                    el.child(
                        div()
                            .id(("file-tree-git-status", ix))
                            .flex_none()
                            .w(px(10.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(marker_color(kind, &theme).opacity(if selected {
                                1.0
                            } else {
                                0.9
                            }))
                            .child(letter)
                            .tooltip(move |_, cx| {
                                cx.new(|_| crate::image_viewer::ViewerTooltip(label.clone()))
                                    .into()
                            }),
                    )
                },
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

        // The drag payload (ticket 09), captured before the row's listeners
        // take the row: a plain/inside-root entry drags into the composer as
        // a path reference carrying its ABSOLUTE path.
        let drag = attachable.then(|| TreeEntryDrag {
            path: row.path.clone(),
            is_dir,
            title: row.name.clone(),
        });
        row_el = row_el
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |_this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let (parent, rename) = match &row.kind {
                        WorkspaceEntryKind::Directory => (row.path.clone(), None),
                        // A file row creates in its parent; a symlinked
                        // directory row creates inside the resolved target.
                        WorkspaceEntryKind::SymlinkInside {
                            target_is_dir: true,
                            resolved_path,
                        } => (resolved_path.clone(), None),
                        _ => {
                            let parent = row
                                .path
                                .trim_end_matches('/')
                                .rsplit_once('/')
                                .map(|(parent, _)| parent.to_string())
                                .unwrap_or_default();
                            (parent, Some((row.path.clone(), row.name.to_string())))
                        }
                    };
                    let rename = rename.or_else(|| {
                        expandable(&row.kind).then(|| (row.path.clone(), row.name.to_string()))
                    });
                    cx.emit(FileTreeEvent::ContextMenu {
                        target: FileMenuTarget {
                            position: event.position,
                            parent,
                            rename,
                            attach: attachable.then(|| row.path.clone()),
                        },
                    });
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                // Files: single click previews, double-click pins (decision
                // 12). Directories toggle on either.
                let pin = event.click_count() >= 2;
                this.activate_row(ix, pin, cx);
            }));
        // The drag source (ticket 09).
        if let Some(drag) = drag {
            row_el = row_el.on_drag(drag, |payload, _point: gpui::Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| TreeEntryGhost {
                    title: payload.title.clone(),
                    is_dir: payload.is_dir,
                })
            });
        }
        row_el.into_any_element()
    }
}

#[cfg(test)]
mod git_status_tests {
    use super::*;
    use holt_proto::{WorkspaceGitStatus, WorkspaceGitStatusEntry, WorkspaceGitStatusKind as Kind};

    fn index(workdir: Option<&str>, entries: &[(&str, Kind)]) -> GitStatusIndex {
        GitStatusIndex::from(&WorkspaceGitStatus {
            workdir: workdir.map(str::to_string),
            entries: entries
                .iter()
                .map(|(path, kind)| WorkspaceGitStatusEntry {
                    path: (*path).to_string(),
                    kind: *kind,
                    index: None,
                    worktree: None,
                    is_dir: false,
                })
                .collect(),
            error: None,
        })
    }

    #[test]
    fn exact_matches_and_clean_entries_classify_directly() {
        let index = index(
            Some("/repo"),
            &[
                ("README.md", Kind::Modified),
                ("debug.log", Kind::Ignored),
                ("staged.rs", Kind::Added),
            ],
        );
        assert_eq!(
            git_kind_for(&index, "/repo/README.md"),
            Some(Kind::Modified)
        );
        assert_eq!(git_kind_for(&index, "/repo/debug.log"), Some(Kind::Ignored));
        assert_eq!(git_kind_for(&index, "/repo/staged.rs"), Some(Kind::Added));
        // A tracked, unchanged file: nothing in the map, no ancestors —
        // clean rows carry no decoration.
        assert_eq!(git_kind_for(&index, "/repo/src/lib.rs"), None);
    }

    #[test]
    fn untracked_and_ignored_directories_classify_descendants() {
        // git reports wholly-untracked/ignored directories as one entry.
        let index = index(
            Some("/repo"),
            &[
                ("node_modules", Kind::Ignored),
                ("prototype", Kind::Untracked),
                ("node_modules/edge", Kind::Modified), // more specific wins
            ],
        );
        assert_eq!(
            git_kind_for(&index, "/repo/node_modules"),
            Some(Kind::Ignored)
        );
        assert_eq!(
            git_kind_for(&index, "/repo/node_modules/react/index.js"),
            Some(Kind::Ignored)
        );
        // An exact deeper match overrides the ancestor's classification.
        assert_eq!(
            git_kind_for(&index, "/repo/node_modules/edge"),
            Some(Kind::Modified)
        );
        assert_eq!(
            git_kind_for(&index, "/repo/prototype/main.rs"),
            Some(Kind::Untracked)
        );
    }

    #[test]
    fn non_git_and_outside_workdir_paths_have_no_decoration() {
        // A non-Git Space: decorations off, never an error state.
        let non_git = index(None, &[]);
        assert_eq!(git_kind_for(&non_git, "/anywhere/file.rs"), None);

        // Paths outside the workdir (outside-root symlink entries spell a
        // different tree) never classify.
        let index = index(Some("/repo"), &[("a.txt", Kind::Untracked)]);
        assert_eq!(git_kind_for(&index, "/elsewhere/a.txt"), None);
    }

    #[test]
    fn a_failed_snapshot_never_decorates() {
        // A git failure is not a clean tree: whatever entries the frame
        // happened to carry, the rows stay plain and the tree usable.
        let failed = GitStatusIndex::from(&WorkspaceGitStatus {
            workdir: Some("/repo".into()),
            entries: vec![WorkspaceGitStatusEntry {
                path: "a.txt".into(),
                kind: Kind::Untracked,
                index: None,
                worktree: Some(Kind::Untracked),
                is_dir: false,
            }],
            error: Some("corrupt index".into()),
        });
        assert_eq!(git_kind_for(&failed, "/repo/a.txt"), None);
    }

    #[test]
    fn hidden_is_not_ignored_and_marker_letters_cover_the_kinds() {
        // The engine classifies hidden-but-untracked entries as plain
        // untracked; the map carries no implicit hidden rule.
        let index = index(Some("/repo"), &[(".env", Kind::Untracked)]);
        assert_eq!(git_kind_for(&index, "/repo/.env"), Some(Kind::Untracked));

        for (kind, letter) in [
            (Kind::Untracked, "U"),
            (Kind::Added, "A"),
            (Kind::Modified, "M"),
            (Kind::Conflicted, "C"),
            (Kind::Deleted, "D"),
        ] {
            assert_eq!(
                marker_parts(kind).map(|(letter, _)| letter),
                Some(letter),
                "{kind:?} keeps its marker letter"
            );
        }
        // Ignored has no letter — subdued styling is its whole treatment.
        assert_eq!(marker_parts(Kind::Ignored), None);
        // Conflicted styles like Modified.
        let theme = Theme::default();
        assert_eq!(
            marker_color(Kind::Conflicted, &theme),
            marker_color(Kind::Modified, &theme)
        );
    }

    #[gpui::test]
    fn status_snapshots_land_on_the_owning_space_only(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        let snapshot = WorkspaceGitStatus {
            workdir: Some("/repo".into()),
            entries: vec![WorkspaceGitStatusEntry {
                path: "notes.md".into(),
                kind: Kind::Modified,
                index: Some(Kind::Modified),
                worktree: None,
                is_dir: false,
            }],
            error: None,
        };

        tree.update(cx, |tree, _| {
            tree.active = Some(ActiveRoot {
                space_key: "space-1".into(),
                chat_id: None,
                space_id: Some("space-1".into()),
                generation: 3,
            });
            tree.spaces
                .entry("space-1".into())
                .or_insert_with(SpaceTree::new);
            tree.spaces
                .entry("space-2".into())
                .or_insert_with(SpaceTree::new);

            // The live frame's key: applied to its owning Space.
            tree.apply_status_frame(3, "space-1", snapshot.clone());
            assert!(tree.spaces.get("space-1").unwrap().git_status.is_some());
            assert!(tree.spaces.get("space-2").unwrap().git_status.is_none());

            // A frame from a previous generation (the Space switched away
            // and back) is stale — dropped, never applied.
            tree.apply_status_frame(2, "space-1", snapshot.clone());
            assert!(tree.spaces.get("space-1").unwrap().git_status.is_some());

            // A frame for another Space's key never lands here either.
            tree.apply_status_frame(3, "space-2", snapshot);
            assert!(tree.spaces.get("space-2").unwrap().git_status.is_none());
        });
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::state::AppState;
    use gpui::TestAppContext;

    fn chat(id: &str, space_id: Option<&str>) -> holt_proto::Chat {
        holt_proto::Chat {
            id: id.into(),
            device_id: "device".into(),
            title: None,
            title_source: holt_proto::TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            space_id: space_id.map(str::to_string),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            approved_plan_path: None,
        }
    }

    fn install_navigation(cx: &mut TestAppContext, expanded: Vec<String>) {
        let record = crate::settings::SpaceFileNavigation {
            tabs: Vec::new(),
            selected: None,
            expanded,
        };
        cx.update(|cx| {
            crate::settings::init(
                {
                    let mut settings = crate::settings::UiSettings::default();
                    settings.file_navigation.insert("space-1".into(), record);
                    settings
                },
                std::env::temp_dir(),
                cx,
            );
        });
    }

    #[gpui::test]
    fn expansion_restores_when_a_chat_selects_the_space(cx: &mut TestAppContext) {
        // The mainline restart path: a chat (belonging to a real Space) is
        // selected. The tree must seed the persisted expansion.
        install_navigation(cx, vec!["/tmp/space-1/src".into()]);
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats = vec![chat("chat-1", Some("space-1"))];
            state.selected_chat = Some("chat-1".into());
            state.selected_space = Some("space-1".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.expanded_for("space-1")),
            Some(vec!["/tmp/space-1/src".to_string()]),
            "chat-selected roots restore their Space's expansion"
        );
    }

    #[gpui::test]
    fn expansion_restores_on_the_canvas_and_skips_cwd_keys(cx: &mut TestAppContext) {
        install_navigation(cx, vec!["/tmp/space-1/docs".into()]);
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.expanded_for("space-1")),
            Some(vec!["/tmp/space-1/docs".to_string()]),
            "the canvas restores too"
        );

        // A `cwd:`-keyed root (spaceless chat) never seeds — even from a
        // stale record stored under that exact key.
        cx.update(|cx| {
            let stale = crate::settings::SpaceFileNavigation {
                tabs: Vec::new(),
                selected: None,
                expanded: vec!["/tmp/solo/src".into()],
            };
            crate::settings::update(crate::settings::SavePolicy::Immediate, cx, |settings| {
                settings
                    .file_navigation
                    .insert("cwd:/tmp/solo".into(), stale);
            });
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats = vec![chat("solo", None)];
            state.selected_chat = Some("solo".into());
            state.chats[0].cwd = Some("/tmp/solo".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.expanded_for("cwd:/tmp/solo")),
            Some(Vec::new()),
            "cwd: keys are session-only — the stale record is ignored"
        );
    }

    #[gpui::test]
    fn collapsing_a_directory_drops_its_descendants_flags(cx: &mut TestAppContext) {
        // Ancestor closure: collapse removes the subtree's expansion flags
        // so nothing reseeds invisibly after a restore.
        install_navigation(cx, Vec::new());
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        tree.update(cx, |tree, _| {
            let active = tree.active.clone().expect("root");
            let space = tree
                .spaces
                .entry(active.space_key)
                .or_insert_with(SpaceTree::new);
            space.expanded.insert("/tmp/space-1/src".into());
            space.expanded.insert("/tmp/space-1/src/deep".into());
            space.expanded.insert("/tmp/space-1/other".into());
        });
        // Rows: with no listings loaded, rows are empty; toggle works off
        // the rows — so inject a row first.
        tree.update(cx, |tree, _| {
            tree.rows.push(TreeRow {
                depth: 0,
                name: "src".into(),
                path: "/tmp/space-1/src".into(),
                kind: WorkspaceEntryKind::Directory,
                marker: None,
            });
        });
        tree.update(cx, |tree, cx| tree.toggle_expansion(0, cx));
        let remaining = tree.read_with(cx, |tree, _| tree.expanded_for("space-1"));
        assert_eq!(
            remaining,
            Some(vec!["/tmp/space-1/other".to_string()]),
            "collapse drops the subtree's flags"
        );
    }

    #[gpui::test]
    fn cut_markers_are_per_space_and_clearable(cx: &mut TestAppContext) {
        install_navigation(cx, Vec::new());
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        tree.update(cx, |tree, _| {
            tree.set_cut("space-1", Some("/tmp/space-1/a.rs"));
            tree.set_cut("space-2", Some("/tmp/space-2/b.rs")); // never visited: no-op
        });
        assert_eq!(
            tree.read_with(cx, |tree, _| tree
                .with_active_space(|space| space.cut.clone()))
                .flatten(),
            Some("/tmp/space-1/a.rs".to_string()),
            "the active Space's cut marks its row"
        );
        tree.update(cx, |tree, _| tree.clear_all_cuts());
        assert_eq!(
            tree.read_with(cx, |tree, _| tree
                .with_active_space(|space| space.cut.clone()))
                .flatten(),
            None,
            "clearing drops every marker — the cut is consumed"
        );
    }

    #[gpui::test]
    fn a_move_or_rename_carries_the_tree_selection(cx: &mut TestAppContext) {
        // Ticket 07: the entry's path rewrite carries the selection — a
        // moved (or renamed) row stays selected, including descendants.
        install_navigation(cx, vec!["/tmp/space-1/src".into()]);
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let tree = cx.new(|cx| FileTreePanel::new(state, cx));
        cx.run_until_parked();
        tree.update(cx, |tree, _| {
            if let Some(active) = tree.active.clone() {
                let space = tree
                    .spaces
                    .entry(active.space_key)
                    .or_insert_with(SpaceTree::new);
                space.selection = Some("/tmp/space-1/src/main.rs".into());
            }
        });
        tree.update(cx, |tree, _| {
            tree.carry_tree_over_rename("/tmp/space-1/src", "/tmp/space-1/source");
        });
        let selection = tree.read_with(cx, |tree, _| tree.selection_path());
        assert_eq!(
            selection,
            Some("/tmp/space-1/source/main.rs".to_string()),
            "the selected descendant follows the move"
        );
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

#[cfg(test)]
mod reveal_tests {
    use super::*;
    use crate::state::AppState;
    use gpui::TestAppContext;
    use holt_proto::WorkspaceEntry;

    fn entry(name: &str, path: &str, kind: WorkspaceEntryKind) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.into(),
            path: path.into(),
            kind,
            size: None,
        }
    }

    fn listing(path: &str, entries: Vec<WorkspaceEntry>) -> DirState {
        DirState::Loaded(WorkspaceListing {
            path: path.into(),
            entries,
            truncated: false,
        })
    }

    fn panel(cx: &mut TestAppContext) -> gpui::Entity<FileTreePanel> {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        cx.new(|cx| FileTreePanel::new(state, cx))
    }

    /// Ticket 09's folder-reveal: `reveal_path` walks the ancestor chain one
    /// level at a time, waiting for each listing, and lands with the target
    /// selected — never treating the folder as editable text.
    #[gpui::test]
    fn reveal_walks_ancestors_and_selects_the_target(cx: &mut TestAppContext) {
        let tree = panel(cx);
        // Root listing holds `src`; `src` is not loaded yet.
        tree.update(cx, |tree, _| {
            tree.spaces.get_mut("space-1").unwrap().dirs.insert(
                String::new(),
                listing(
                    "/tmp/space-1",
                    vec![
                        entry("src", "/tmp/space-1/src", WorkspaceEntryKind::Directory),
                        entry(
                            "README.md",
                            "/tmp/space-1/README.md",
                            WorkspaceEntryKind::File,
                        ),
                    ],
                ),
            );
        });
        // No engine in this test, so a missing listing never loads by itself:
        // the walk pauses at `src` until its listing lands.
        tree.update(cx, |tree, cx| {
            tree.reveal_path("/tmp/space-1/src/deep/x.rs", cx)
        });
        tree.update(cx, |tree, _| {
            let space = tree.spaces.get("space-1").unwrap();
            assert!(
                space.expanded.contains("/tmp/space-1/src"),
                "the ancestor expands while waiting for its listing"
            );
            assert_eq!(
                tree.pending_reveal.as_deref(),
                Some("/tmp/space-1/src/deep/x.rs")
            );
            assert!(tree.selection_path().is_none(), "nothing selected yet");
        });
        // The `src` listing arrives (deep is a dir, x.rs the target).
        tree.update(cx, |tree, _| {
            tree.spaces.get_mut("space-1").unwrap().dirs.insert(
                "/tmp/space-1/src".into(),
                listing(
                    "/tmp/space-1/src",
                    vec![
                        entry(
                            "deep",
                            "/tmp/space-1/src/deep",
                            WorkspaceEntryKind::Directory,
                        ),
                        entry(
                            "other.rs",
                            "/tmp/space-1/src/other.rs",
                            WorkspaceEntryKind::File,
                        ),
                    ],
                ),
            );
        });
        // `deep` is still missing — the walk advances one level and pauses.
        tree.update(cx, |tree, cx| tree.advance_reveal(cx));
        tree.update(cx, |tree, _| {
            assert!(
                tree.spaces
                    .get("space-1")
                    .unwrap()
                    .expanded
                    .contains("/tmp/space-1/src/deep")
            );
            assert!(tree.pending_reveal.is_some());
        });
        tree.update(cx, |tree, _| {
            tree.spaces.get_mut("space-1").unwrap().dirs.insert(
                "/tmp/space-1/src/deep".into(),
                listing(
                    "/tmp/space-1/src/deep",
                    vec![entry(
                        "x.rs",
                        "/tmp/space-1/src/deep/x.rs",
                        WorkspaceEntryKind::File,
                    )],
                ),
            );
        });
        tree.update(cx, |tree, cx| tree.advance_reveal(cx));
        tree.update(cx, |tree, _| {
            assert_eq!(
                tree.selection_path().as_deref(),
                Some("/tmp/space-1/src/deep/x.rs"),
                "the target row is selected once visible"
            );
            assert!(tree.pending_reveal.is_none(), "the reveal finished");
        });
    }

    /// A target that left the tree since the search saw it walks as deep as
    /// it can and gives up without leaving dead expansion flags behind.
    #[gpui::test]
    fn reveal_of_a_missing_entry_stops_cleanly(cx: &mut TestAppContext) {
        let tree = panel(cx);
        tree.update(cx, |tree, _| {
            tree.spaces
                .get_mut("space-1")
                .unwrap()
                .dirs
                .insert(String::new(), listing("/tmp/space-1", Vec::new()));
        });
        // `src` never exists in the root's listing, but its listing request
        // fails (no engine → Missing → wait). Inject a Failed listing to
        // simulate the engine refusing the missing directory.
        tree.update(cx, |tree, cx| tree.reveal_path("/tmp/space-1/src/x.rs", cx));
        tree.update(cx, |tree, _| {
            tree.spaces
                .get_mut("space-1")
                .unwrap()
                .dirs
                .insert("/tmp/space-1/src".into(), DirState::Failed("nope".into()));
        });
        tree.update(cx, |tree, cx| tree.advance_reveal(cx));
        tree.update(cx, |tree, _| {
            assert!(tree.pending_reveal.is_none());
            assert!(
                !tree
                    .spaces
                    .get("space-1")
                    .unwrap()
                    .expanded
                    .contains("/tmp/space-1/src"),
                "the failed ancestor's expansion flag is dropped"
            );
        });
    }

    /// A target outside the current root (a stale reveal after a Space
    /// switch) is refused outright.
    #[gpui::test]
    fn reveal_outside_the_root_is_refused(cx: &mut TestAppContext) {
        let tree = panel(cx);
        tree.update(cx, |tree, _| {
            tree.spaces
                .get_mut("space-1")
                .unwrap()
                .dirs
                .insert(String::new(), listing("/tmp/space-1", Vec::new()));
        });
        tree.update(cx, |tree, cx| tree.reveal_path("/elsewhere/pkg", cx));
        tree.update(cx, |tree, _| {
            assert!(tree.pending_reveal.is_none());
            assert!(tree.spaces.get("space-1").unwrap().expanded.is_empty());
        });
    }
}
