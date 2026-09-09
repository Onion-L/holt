//! The app shell (holt `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is holt's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360px floor, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Empty, Entity, Focusable as _, IntoElement,
    KeyBinding, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, SharedString, Subscription, Task, Window, WindowControlArea, actions,
    div, prelude::*, px,
};

use holt_rpc::methods;

use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::files::FileStateMap;
use crate::files::tree::{FileMenuTarget, FileTreeEvent, FileTreePanel};
use crate::files::viewer::{FileScope, FileViewerEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, SPLASH_OUT, TAB_SLIDE};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::providers::{ProvidersPage, ProvidersPageEvent};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    self, CHAT_PANEL_MIN, FILE_TREE_DEFAULT, FILE_TREE_MIN, JUMP_SLOTS, KeymapConfig,
    RIGHT_PANE_DEFAULT, RIGHT_PANE_MIN, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, SavePolicy,
    ShortcutId, SidebarSort, TERMINAL_DEFAULT_HEIGHT, UiSettings, badge_combo, jump_hints_visible,
    platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator, format_time_ago,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript, TranscriptEvent};

mod chat_list;
mod chat_menu;
mod right_pane;
mod spaces;
mod tabs;
mod titlebar;

pub use chat_list::*;
use chat_menu::ChatMenuState;
mod file_lookup;
mod file_sidebar;
use file_sidebar::*;
pub use right_pane::*;
use spaces::{AddSpaceFlow, RenameSpaceDialog, SidebarDisclosureMotion};
pub use titlebar::*;

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        OpenFileLookup,
        NewSession,
        OpenSettings,
        NextSession,
        PrevSession,
        ArchiveSession
    ]
);

/// Vertical pane resize hitboxes yield the global titlebar. Keeping this in
/// the shared constructor makes left/right seams mirror each other and avoids
/// relying on paint order when chrome crosses an animated pane boundary.
const PANE_RESIZE_HITBOX_TOP: f32 = Theme::TITLEBAR_HEIGHT;

fn stable_panel_content_width(target: f32, transition: Option<(f32, f32)>) -> f32 {
    transition.map(|(from, to)| from.max(to)).unwrap_or(target)
}

fn right_panel_content_width(
    target: f32,
    transition: Option<(f32, f32)>,
    takeover_width: Option<f32>,
) -> f32 {
    takeover_width.unwrap_or_else(|| stable_panel_content_width(target, transition))
}

fn conversation_width(viewport: f32, sidebar: f32, right: f32) -> f32 {
    (viewport - sidebar - right).max(0.0)
}

/// Random index into `loaders::MARK_SHAPES` — time-seeded, display-only (no
/// crypto need). Re-rolled per new-chat canvas visit.
fn random_mark_index() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0)
        % loaders::MARK_SHAPES.len()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExternalApp {
    Finder,
    VsCode,
    Cursor,
    Zed,
    PyCharm,
    Terminal,
    Ghostty,
}

impl ExternalApp {
    fn label(self) -> &'static str {
        match self {
            Self::Finder => "Finder",
            Self::VsCode => "VS Code",
            Self::Cursor => "Cursor",
            Self::Zed => "Zed",
            Self::PyCharm => "PyCharm",
            Self::Terminal => "Terminal",
            Self::Ghostty => "Ghostty",
        }
    }

    fn bundle_name(self) -> Option<&'static str> {
        match self {
            Self::Finder => None,
            Self::VsCode => Some("Visual Studio Code"),
            Self::Cursor => Some("Cursor"),
            Self::Zed => Some("Zed"),
            Self::PyCharm => Some("PyCharm"),
            Self::Terminal => Some("Terminal"),
            Self::Ghostty => Some("Ghostty"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Finder => icons::FOLDER,
            Self::Terminal | Self::Ghostty => icons::TERMINAL,
            _ => icons::PROGRAMMING_OUTLINE,
        }
    }
}

fn available_external_apps() -> Vec<ExternalApp> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    [
        ExternalApp::Finder,
        ExternalApp::VsCode,
        ExternalApp::Cursor,
        ExternalApp::Zed,
        ExternalApp::PyCharm,
        ExternalApp::Terminal,
        ExternalApp::Ghostty,
    ]
    .into_iter()
    .filter(|app| external_app_installed(*app))
    .collect()
}

fn external_app_installed(app: ExternalApp) -> bool {
    let candidates: &[&str] = match app {
        ExternalApp::Finder => &["/System/Library/CoreServices/Finder.app"],
        ExternalApp::VsCode => &["/Applications/Visual Studio Code.app"],
        ExternalApp::Cursor => &["/Applications/Cursor.app"],
        ExternalApp::Zed => &["/Applications/Zed.app"],
        ExternalApp::PyCharm => &["/Applications/PyCharm.app", "/Applications/PyCharm CE.app"],
        ExternalApp::Terminal => &["/System/Applications/Utilities/Terminal.app"],
        ExternalApp::Ghostty => &["/Applications/Ghostty.app"],
    };
    candidates.iter().any(|path| Path::new(path).exists())
        || std::env::var_os("HOME").is_some_and(|home| {
            candidates.iter().any(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| Path::new(&home).join("Applications").join(name).exists())
                    .unwrap_or(false)
            })
        })
}

/// Open the session at `slot` (zero-based) of the sidebar's active list. One
/// action carrying the slot, rather than nine near-identical action types.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct JumpSession(pub usize);

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (Settings on every platform; ⌘Q quit, ⌘W
    // close, ⌘M minimize, ⌘H hide on macOS) — these back the native menu
    // key equivalents and must survive keymap re-application.
    crate::app_menus::bind_keys(cx);
    crate::terminal::panel::init(cx);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(
                &keymap.toggle_sidebar,
                crate::settings::ShortcutId::ToggleSidebar.default_combo(),
            ),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.toggle_changes,
                crate::settings::ShortcutId::ToggleChanges.default_combo(),
            ),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.new_session, "mod-n"),
            NewSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.next_session,
                crate::settings::ShortcutId::NextSession.default_combo(),
            ),
            NextSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.prev_session,
                crate::settings::ShortcutId::PrevSession.default_combo(),
            ),
            PrevSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.archive_session, "mod-shift-a"),
            ArchiveSession,
            None,
        ),
        // Fixed: ⌘K summons the add-space palette (the ⌘K chip in its search
        // bar); pressing it again dismisses.
        KeyBinding::new(&platform_combo("mod-k"), AddSpacePalette, None),
        // Fixed: ⌘P opens the find-file palette (ticket 09) — the tree
        // column's search row carries the same shortcut.
        KeyBinding::new(&platform_combo("mod-p"), OpenFileLookup, None),
    ]);
    // ⌘1..⌘9 open the sidebar's first nine rows. A slot left unbound (an empty
    // combo in a hand-edited file) binds nothing rather than falling back —
    // the user cleared it on purpose.
    cx.bind_keys((0..JUMP_SLOTS).filter_map(|slot| {
        let id = ShortcutId::JumpSession(slot);
        let combo = keymap.get(id);
        if combo.is_empty() {
            return None;
        }
        Some(KeyBinding::new(
            &valid_or_default(combo, id.default_combo()),
            JumpSession(slot),
            None,
        ))
    }));
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Providers,
    Appearance,
    Shortcuts,
    Skills,
    Archived,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 6] = [
        SettingsSection::General,
        SettingsSection::Providers,
        SettingsSection::Appearance,
        SettingsSection::Shortcuts,
        SettingsSection::Skills,
        SettingsSection::Archived,
    ];

    /// Sidebar + header label (holt settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Providers => "Providers",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Skills => "Skills",
            SettingsSection::General => "General",
            SettingsSection::Archived => "Archived sessions",
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
}

/// One route-history entry (holt parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (holt window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (holt's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (holt window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// One right-pane subagent tab: the doc it shows, its strip title, and the
/// read-only transcript entity whose drop tears the view down.
struct SubagentTab {
    doc_id: String,
    title: SharedString,
    transcript: Entity<Transcript>,
    /// Keeps a frozen-blob fetch alive (it falls back to a live doc watch).
    _fetch: Option<Task<()>>,
    /// Spawn chips INSIDE the subagent transcript open their own tabs.
    _events: Subscription,
}

/// Semantic kind for a holt notice — drives the icon + color tokens used
/// to render the chip. Each kind pairs with a `(border/icon, text)` color
/// pair so error reads as urgent, success as affirming, etc.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HoltNoticeKind {
    /// Routine status (e.g. "Copying…", deep-link echoes). Brand-accent
    /// tint — informational but not alarming.
    Plain,
    /// Completed an action the user asked for (copy succeeded).
    Success,
    /// Heads-up that something is wrong or might fail (missing data,
    /// deprecation). Amber tint — softer than error.
    Warning,
    /// Action failed. Red tint — the most attention-demanding kind.
    Error,
}

/// One stacked top-right notification chip — replaces the inline sidebar
/// notice strip. Each entry owns its own auto-dismiss timer; the timer is
/// dropped (canceled) while hovered and re-armed on unhover. Manual
/// dismissal is also exposed via the close button.
struct HoltNotice {
    /// Stable per-notice id — lets listeners and animations key on the
    /// specific entry even when several stack at once.
    id: u64,
    kind: HoltNoticeKind,
    message: SharedString,
    /// Whether the pointer is currently over the chip. Drives the
    /// hover-pause contract for the auto-dismiss timer.
    hovered: bool,
    /// 2s auto-dismiss. Dropped (and therefore canceled) when the chip
    /// becomes hovered, re-armed on unhover, and on manual close.
    timer: Option<Task<()>>,
}

/// Color pair used to paint a holt notice chip: `(border/icon, text)`.
/// Kept local to the shell since the pairing is a chip-specific decision,
/// not a generic theme contract.
fn holt_notice_palette(kind: HoltNoticeKind, theme: &Theme) -> (gpui::Hsla, gpui::Hsla) {
    match kind {
        // Plain uses the brand accent — informational without alarm.
        HoltNoticeKind::Plain => (theme.accent, theme.accent.opacity(0.85)),
        HoltNoticeKind::Success => (theme.success, theme.success_muted),
        HoltNoticeKind::Warning => (theme.warning, theme.warning_muted),
        HoltNoticeKind::Error => (theme.danger, theme.danger_muted),
    }
}

/// Icon glyph for each notice kind. Warning and error share the triangle
/// so the *color* — not the shape — carries the severity distinction
/// (matches the rest of the app, e.g. provider_error).
fn holt_notice_icon(kind: HoltNoticeKind) -> &'static str {
    match kind {
        HoltNoticeKind::Plain => icons::INFO_CIRCLE,
        HoltNoticeKind::Success => icons::CHECK,
        HoltNoticeKind::Warning => icons::DANGER_TRIANGLE,
        HoltNoticeKind::Error => icons::DANGER_TRIANGLE,
    }
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Measured height of the bottom chrome stack (status strip + composer)
    /// the full-height transcript scrolls under — written by a
    /// paint-time canvas each frame, read the NEXT frame for the fade inset,
    /// the transcript's bottom clearance, and the jump pill's anchor (the
    /// same one-frame lag every fade here rides).
    bottom_stack: std::rc::Rc<std::cell::Cell<f32>>,
    /// Ephemeral collapsed project sections, keyed by organization + id.
    pub(super) sidebar_collapsed_groups: std::collections::HashSet<String>,
    /// In-flight disclosure tweens, shared by the project groups.
    pub(super) sidebar_disclosure_motion:
        std::collections::HashMap<String, SidebarDisclosureMotion>,
    /// The jump-hint overlay: true while the held modifiers exactly match a
    /// jump shortcut, which swaps the first nine rows' time-ago for their
    /// key-cap chip (t3code's `showJumpHints`). Frame-transient — window
    /// deactivation clears it, so a chip cannot stick after an app switch
    /// swallows the key-up.
    pub(super) jump_hints: bool,
    /// Which `loaders::MARK_SHAPES` variant the new-chat canvas shows —
    /// re-rolled at random each time a chat is selected, so the next visit
    /// to the bare canvas gets a fresh shape.
    new_chat_mark: usize,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    /// Embedded terminal host for right-pane Terminal surfaces — a SEPARATE
    /// entity from the bottom drawer's (own PTYs, own grid geometry; one
    /// panel can only size one visible grid at a time).
    right_terminal: Option<Entity<TerminalPanel>>,
    /// The surface-tab strip's `+` menu (Terminal / Git diff rows).
    right_plus: popover::Popup<()>,
    /// External workspace application picker in the session titlebar.
    external_app_menu: popover::Popup<ExternalApp>,
    /// Applications detected on the host and offered by the picker.
    external_apps: Vec<ExternalApp>,
    /// Diff surfaces by id — each tab its own [`Changes`] viewer with its own
    /// scope/base pick and diff watch (multiple diff panels, user request).
    diffs: std::collections::HashMap<u64, Entity<Changes>>,
    /// Event hookups for [`Self::diffs`] (History rows opening commit tabs).
    diff_subs: std::collections::HashMap<u64, Subscription>,
    diff_seq: u64,
    /// Subagent transcript surfaces by id — each tab a read-only
    /// [`Transcript`] pinned to its subagent doc.
    subagent_tabs: std::collections::HashMap<u64, SubagentTab>,
    subagent_seq: u64,
    /// Ordered surface tabs per panel key (drag-reorderable; stale entries —
    /// closed terminals/diffs — are skipped at read time).
    right_tabs: std::collections::HashMap<String, Vec<RightSurface>>,
    /// In-flight surface-tab drag (slide animation state).
    right_tab_drag: Option<RightTabDragState>,
    /// Surface-tab strip scroll (the strip overflows horizontally, t3
    /// ScrollArea-style; drag drop-math reads the offset back out).
    right_tab_scroll: gpui::ScrollHandle,
    /// The far-right File tree panel (lazy: no entity until first shown).
    file_tree: Option<Entity<crate::files::tree::FileTreePanel>>,
    /// Whether the File tree column shows. Session state (the width is the
    /// persisted part); default visible — Chat | contents | tree is the
    /// feature's reference arrangement.
    file_tree_visible: bool,
    /// The File tree column's width tween.
    file_tree_tween: Option<WidthTween>,
    /// Space-owned file tabs (ADR-0020): open tabs, their viewers, and the
    /// strip's selected tab, keyed by space.
    file_state: FileStateMap,
    /// Event hookups for open file viewers (resolved-path bookkeeping).
    file_viewers_sub: std::collections::HashMap<u64, Subscription>,
    /// A modified file tab awaiting its close decision (Save/Discard/
    /// Cancel), bound to the space it lived in when the dialog opened — a
    /// switch mid-dialog cannot strand or misapply it.
    dirty_file_close: Option<(String, u64)>,
    /// A space whose removal is gated on its modified file tabs.
    dirty_space_close: Option<String>,
    /// Tabs whose close is held until their in-flight save settles (a failed
    /// save keeps the tab — the draft must survive).
    closing_after_save: std::collections::HashSet<u64>,
    /// Whether the quit lifecycle already holds this shell's entity.
    lifecycle_attached: bool,
    /// The tree panel's open-request subscription.
    _file_tree_events: Option<Subscription>,
    /// The tree panel's expansion-persistence observation.
    _file_tree_expansion: Option<Subscription>,
    /// The File tree's context menu (ticket 06): the menu target plus the
    /// owning Space it opened under.
    file_menu: popover::Popup<(FileMenuTarget, String)>,
    /// The find-file palette (ticket 09, ⌘P), `Some` while open.
    file_lookup: Option<file_lookup::FileLookup>,
    /// The create/rename dialog opened from that menu.
    file_op_dialog: Option<file_sidebar::FileOpDialog>,
    /// Bumped whenever a new file-op dialog opens — stale in-flight errors
    /// from a previous dialog never land in the current one.
    file_op_epoch: u64,
    /// The File tree's pending cut (ticket 07): the entry, its display
    /// name, and the Space it was cut in. A paste never acts on a
    /// different Space after navigation.
    file_cut: Option<file_sidebar::FileCut>,
    /// A trash request held for its Save-all/Discard-all/Cancel decision
    /// (ticket 07), bound to the Space it came from.
    file_trash_confirm: Option<file_sidebar::FileTrashConfirm>,
    /// Id mint for file tabs.
    file_seq: u64,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    providers_page: Option<Entity<ProvidersPage>>,
    providers_sub: Option<Subscription>,
    skills_page: Option<Entity<crate::settings::skills::SkillsPage>>,
    general_page: Option<Entity<crate::settings::general::GeneralPage>>,
    /// Last action failure from the providers page, shown as the window-top
    /// error alert until its 2s timer fires or the close button is pressed.
    provider_error: Option<SharedString>,
    /// The auto-dismiss timer. Replaced on every new error and dropped on
    /// manual dismissal — dropping a `Task` cancels it, so no epoch guard.
    provider_error_timer: Option<Task<()>>,
    shortcuts_sub: Option<Subscription>,
    /// Session-row context menu, including the Copy submenu.
    chat_menu: popover::Popup<ChatMenuState>,
    chat_copy_task: Option<Task<()>>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Chat id awaiting archive confirmation.
    archive_confirm: Option<String>,
    /// Space-row context menu (dropdown rows): (space id, window position).
    space_menu: popover::Popup<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// The sidebar's space-filter dropdown.
    spaces_menu: popover::Popup<spaces::SpacesMenu>,
    /// Natural-tab-order focus target for the icon-only view-options button.
    sidebar_view_trigger_focus: gpui::FocusHandle,
    /// Chat id whose STATUS CORNER is under the pointer — just that corner
    /// swaps to the archive button (t3code's settle-on-hover); hovering the
    /// row body leaves the status readable.
    chat_status_hover: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Stacked top-right notification chips — replaces the inline sidebar
    /// notice strip. Each entry owns its own 2s auto-dismiss timer that's
    /// paused on hover. Click the × to dismiss early.
    holt_notices: Vec<HoltNotice>,
    next_holt_notice_id: u64,
    mutate_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`HOLT_OPEN_DIALOG`, `HOLT_FORCE_GATE`,
    /// `HOLT_DEMO_UPLOAD`) — see [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    debug_upload: Option<String>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    /// Mirrors `right_tween` only for takeover entry/exit, allowing the visible
    /// right-panel contents to resize with their outer frame in that mode.
    right_takeover_content_tween: Option<WidthTween>,
    /// Conversation-width tween used only while entering/leaving right-pane
    /// takeover. Normal right-pane open/close keeps the upstream flex behavior.
    main_takeover_tween: Option<WidthTween>,
    /// Changes-panel takeover (the header's expand button): the panel fills
    /// everything right of the sidebar and the conversation column collapses
    /// to zero. Session-local view state — never persisted, reset on close.
    right_pane_expanded: bool,
    /// Viewport width stamped each frame at render — the expanded panel's
    /// width target and the physical ceiling for free-form resizing
    /// ([`Self::right_target`] has no `Window`).
    viewport_width: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// The caption buttons holt itself draws on Linux under client-side
    /// decorations, per side, already filtered to what the compositor
    /// supports — `None` off Linux or under server decorations (where the WM
    /// draws real buttons). Re-resolved every frame at the top of `render`.
    linux_captions: Option<gpui::WindowButtonLayout>,
    /// Re-renders when the desktop's button layout changes (GNOME
    /// `button-layout` gsetting). Registered on first paint — [`Shell::new`]
    /// has no window.
    button_layout_sub: Option<Subscription>,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// Clears the jump hints when the window deactivates: a Cmd+Tab away
    /// swallows the key-up, so without this the chips stay on screen for good.
    activation_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    /// The primary transcript's spawn-chip events (subagent tabs).
    _transcript_events: Subscription,
}

/// The `UiSettings` fields the Shell owns and publishes through
/// [`Shell::schedule_save`]. Everything else in the record (notification
/// toggles, disabled skills, file navigation, diff layout, …) belongs to
/// other writers and must survive a Shell save untouched.
struct ShellSettingsFields {
    sidebar_width: f32,
    sidebar_collapsed: bool,
    terminal_height: f32,
    right_pane_width: f32,
    file_tree_width: f32,
    last_space_id: Option<String>,
    space_filter: Option<String>,
    keymap: settings::KeymapConfig,
    external_app: String,
    appearance: crate::appearance::AppearanceMode,
    theme_selection: holt_theme::ThemeSelection,
    accent: holt_theme::AccentSelection,
    surface: holt_theme::SurfacePreference,
    ui_font_family: crate::typography::UiFontFamily,
    ui_font_size: crate::typography::UiFontSize,
}

impl ShellSettingsFields {
    fn capture(settings: &UiSettings) -> Self {
        Self {
            sidebar_width: settings.sidebar_width,
            sidebar_collapsed: settings.sidebar_collapsed,
            terminal_height: settings.terminal_height,
            right_pane_width: settings.right_pane_width,
            file_tree_width: settings.file_tree_width,
            last_space_id: settings.last_space_id.clone(),
            space_filter: settings.space_filter.clone(),
            keymap: settings.keymap.clone(),
            external_app: settings.external_app.clone(),
            appearance: settings.appearance,
            theme_selection: settings.theme_selection.clone(),
            accent: settings.accent,
            surface: settings.surface,
            ui_font_family: settings.ui_font_family.clone(),
            ui_font_size: settings.ui_font_size,
        }
    }

    fn apply(self, current: &mut UiSettings) {
        current.sidebar_width = self.sidebar_width;
        current.sidebar_collapsed = self.sidebar_collapsed;
        current.terminal_height = self.terminal_height;
        current.right_pane_width = self.right_pane_width;
        current.file_tree_width = self.file_tree_width;
        current.last_space_id = self.last_space_id;
        current.space_filter = self.space_filter;
        current.keymap = self.keymap;
        current.external_app = self.external_app;
        current.appearance = self.appearance;
        current.theme_selection = self.theme_selection;
        current.accent = self.accent;
        current.surface = self.surface;
        current.ui_font_family = self.ui_font_family;
        current.ui_font_size = self.ui_font_size;
    }
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Every send glides the prompt to the viewport top and reserves the
        // reply's space below it (notes-app parity).
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
            }
        });
        // Spawn chips open their subagent's transcript as a right-pane tab.
        let transcript_events = cx.subscribe(&transcript, Self::on_transcript_event);
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                            // The connection pill's retry countdown needs the
                            // same per-second refresh while degraded.
                            || matches!(
                                s.connectivity.state,
                                holt_proto::ConnectivityState::Offline
                                    | holt_proto::ConnectivityState::Reconnecting
                            )
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let settings = settings::current(cx);
        state.update(cx, |state, cx| {
            state.set_change_requests_visible(settings.sidebar_show_pull_request, cx)
        });
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `HOLT_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("HOLT_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/general") => {
                Route::Settings(SettingsSection::General)
            }
            Some("settings/providers") => Route::Settings(SettingsSection::Providers),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/skills") => Route::Settings(SettingsSection::Skills),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `HOLT_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined provider/model menu once the shell is Ready;
        // `HOLT_FORCE_GATE=failed` renders that gate regardless of real
        // connection state (display-only — for styling passes).
        let debug_dialog = std::env::var("HOLT_OPEN_DIALOG").ok();
        // `HOLT_DEMO_UPLOAD=<pct>:<image path>` fabricates an in-flight image
        // send on the selected chat (echo bubble + frozen thumbnail progress
        // ring) — display-only; a real upload can't be paused for a capture.
        let debug_upload = std::env::var("HOLT_DEMO_UPLOAD").ok();
        let debug_gate = match std::env::var("HOLT_FORCE_GATE").ok().as_deref() {
            Some("failed") => Some(GatePhase::Failed("Could not reach the holt engine".into())),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        Self {
            state,
            transcript,
            composer,
            // Seed with the compact composer stack's rough height so the
            // first frame's clearance isn't zero (the measure corrects it).
            bottom_stack: std::rc::Rc::new(std::cell::Cell::new(120.0)),
            sidebar_collapsed_groups: std::collections::HashSet::new(),
            sidebar_disclosure_motion: std::collections::HashMap::new(),
            jump_hints: false,
            new_chat_mark: random_mark_index(),
            terminal: None,
            right_terminal: None,
            right_plus: popover::Popup::default(),
            external_app_menu: popover::Popup::default(),
            external_apps: available_external_apps(),
            diffs: std::collections::HashMap::new(),
            diff_subs: std::collections::HashMap::new(),
            diff_seq: 0,
            subagent_tabs: std::collections::HashMap::new(),
            subagent_seq: 0,
            right_tabs: std::collections::HashMap::new(),
            right_tab_drag: None,
            right_tab_scroll: gpui::ScrollHandle::new(),
            file_tree: None,
            file_tree_visible: true,
            file_tree_tween: None,
            file_state: FileStateMap::default(),
            file_viewers_sub: std::collections::HashMap::new(),
            _file_tree_expansion: None,
            file_menu: popover::Popup::default(),
            file_lookup: None,
            file_op_dialog: None,
            file_op_epoch: 0,
            file_cut: None,
            file_trash_confirm: None,
            dirty_file_close: None,
            dirty_space_close: None,
            closing_after_save: std::collections::HashSet::new(),
            lifecycle_attached: false,
            _file_tree_events: None,
            file_seq: 0,
            route,
            nav,
            archived_page: None,
            appearance_page: None,
            shortcuts_page: None,
            providers_page: None,
            providers_sub: None,
            skills_page: None,
            general_page: None,
            provider_error: None,
            provider_error_timer: None,
            shortcuts_sub: None,
            chat_menu: popover::Popup::default(),
            chat_copy_task: None,
            rename_dialog: None,
            delete_confirm: None,
            archive_confirm: None,
            space_menu: popover::Popup::default(),
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            spaces_menu: popover::Popup::default(),
            sidebar_view_trigger_focus: cx.focus_handle().tab_stop(true),
            chat_status_hover: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            holt_notices: Vec::new(),
            next_holt_notice_id: 0,
            mutate_task: None,
            boot,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            debug_upload,
            sidebar_tween: None,
            right_tween: None,
            right_takeover_content_tween: None,
            main_takeover_tween: None,
            right_pane_expanded: false,
            viewport_width: 1280.0,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            linux_captions: None,
            button_layout_sub: None,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            focus_sub: None,
            activation_sub: None,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        // A Space's persisted file navigation restores the first time the
        // selection lands on it (ticket 05); later frames are no-ops.
        if let Some(space) = self.file_space_key(cx) {
            self.restore_file_navigation_if_needed(&space, cx);
        }
        if let Some(notice) = state.update(cx, |state, _| state.take_deep_link_notice()) {
            self.push_holt_notice(HoltNoticeKind::Plain, notice.into(), cx);
        }
        // Capture knob: the add-space palette opens as soon as it is requested.
        if self.debug_dialog.as_deref() == Some("add-space") {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Capture knob: `HOLT_DEMO_UPLOAD=<pct>:<image path>` — once a chat
        // is selected, push a fake sending echo carrying that image as a
        // pending attachment and freeze upload progress at <pct>, so the
        // thumbnail progress ring can be styled/screenshotted (a real upload
        // is too fast to pause).
        if let Some(spec) = self.debug_upload.clone()
            && let Some(chat_id) = state.read(cx).selected_chat.clone()
        {
            self.debug_upload = None;
            if let Some((pct, img_path)) = spec.split_once(':')
                && let Ok(pct) = pct.parse::<u64>()
                && std::path::Path::new(img_path).is_file()
            {
                let pending_path = img_path.to_string();
                let text = crate::attachments::with_attachments(
                    "Here is the screenshot of the bug.",
                    std::slice::from_ref(&pending_path),
                );
                let echo = holt_doc::SessionMessageEntry {
                    id: "demo-upload-echo".into(),
                    role: holt_doc::MessageRole::User,
                    parts: vec![holt_doc::MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: chrono::Utc::now().timestamp_millis(),
                    device_id: "local".into(),
                    status: None,
                    continuation_of: None,
                };
                state.update(cx, |s, cx| {
                    s.push_echo(&chat_id, echo);
                    s.begin_upload_progress(
                        100,
                        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pct)),
                    );
                    cx.notify();
                });
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // canvas defaults (project) follow it, even
                // over a remembered "no project" opt-out. Otherwise the last
                // selected project stands, unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(&exists);
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        self.settings.last_space_id.clone().filter(&exists)
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // holt's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.right_takeover_content_tween = None;
            self.main_takeover_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| {
                    panel.set_resize_suspended(false);
                    panel.set_open(panels.terminal_open, cx);
                });
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. The new-session canvas keys
    /// its own flag per space: the pane never pops open there by itself, but
    /// the canvas now carries the contents/tree toggles and an explicit file
    /// open reveals the pane (File sidebar, decision 2).
    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if !self.right_pane_open(cx) {
            0.0
        } else {
            // Manual sizing preserves a usable conversation column. Takeover
            // intentionally consumes it completely. Both ride the sidebar
            // tween so toggling it remains seamless.
            let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            // The tree column keeps its own budget beside the pane — the tree
            // hides independently, never as a side effect of the contents
            // pane growing (decision 8).
            let tree = self.file_tree_target(cx);
            if self.right_pane_expanded {
                right_pane_takeover_width(self.viewport_width, sidebar_now) - tree
            } else {
                self.settings
                    .right_pane_width
                    .min(right_pane_max_width(self.viewport_width, sidebar_now) - tree)
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git gate: the pane hosts terminals too (see `right_pane_open`).
        let from = self.right_target(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let from_main = conversation_width(self.viewport_width, sidebar_now, from);
        let was_expanded = self.right_pane_expanded;
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if !open {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        let to = self.right_target(cx);
        self.right_tween = Some(WidthTween::new(from, to));
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = was_expanded.then(|| {
            WidthTween::new(
                from_main,
                conversation_width(self.viewport_width, sidebar_now, to),
            )
        });
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (holt `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| {
            panel.set_resize_suspended(false);
            panel.set_open(open, cx);
        });
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (holt terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in holt's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // No arbitrary percentage ceiling, but retain the chat's usable 300px
        // floor instead of allowing the conversation to collapse to zero.
        // The tree column keeps its own budget beside the pane.
        let max = right_pane_max_width(viewport, self.sidebar_target()) - self.file_tree_target(cx);
        self.settings.right_pane_width = if max >= RIGHT_PANE_MIN {
            width.clamp(RIGHT_PANE_MIN, max)
        } else {
            max
        };
        self.right_tween = None;
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Publish this view's working copy to the central settings store. The
    /// store owns the single debounce task and the only production writer.
    /// Merge ONLY the fields this view owns — a whole-record replace would
    /// let a stale Shell snapshot roll back choices another writer persisted
    /// seconds ago (notification toggles, disabled skills, file navigation).
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        self.settings.ui_font_family = crate::typography::requested(cx);
        self.settings.ui_font_size = crate::typography::font_size(cx);
        let owned = ShellSettingsFields::capture(&self.settings);
        settings::update(SavePolicy::Debounced, cx, move |current| {
            owned.apply(current);
        });
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    /// Close the session-row context menu through the exit animation.
    fn close_chat_menu(&mut self, cx: &mut Context<Self>) {
        if self.chat_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.chat_menu);
            cx.notify();
        }
    }

    fn copy_holt_chat_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let link = {
            let state = self.state.read(cx);
            crate::links::workspace_locator(
                state.workspace_scope,
                state.auth.as_ref(),
                state.local_device_id.as_deref(),
            )
            .map(|workspace| crate::links::holt_chat_link(chat_id, &workspace))
        };
        if let Some(link) = link {
            cx.write_to_clipboard(ClipboardItem::new_string(link));
            self.push_holt_notice(HoltNoticeKind::Success, "Holt Chat link copied".into(), cx);
        } else {
            self.push_holt_notice(
                HoltNoticeKind::Warning,
                "Chat link is not ready yet".into(),
                cx,
            );
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Raise the top-center error alert and arm its 2s auto-dismiss. Each
    /// new error replaces the timer, so a rapid error never inherits a
    /// previous (shorter) deadline.
    fn show_provider_error(&mut self, message: SharedString, cx: &mut Context<Self>) {
        self.provider_error = Some(message);
        self.provider_error_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(2000))
                .await;
            this.update(cx, |this, cx| {
                this.provider_error = None;
                this.provider_error_timer = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Push a top-right holt notice and arm its 2s auto-dismiss timer.
    /// Multiple notices stack vertically and coexist; each owns its own
    /// timer. Replaces the inline `sidebar_notice` strip.
    fn push_holt_notice(
        &mut self,
        kind: HoltNoticeKind,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_holt_notice_id;
        self.next_holt_notice_id += 1;
        self.holt_notices.push(HoltNotice {
            id,
            kind,
            message,
            hovered: false,
            timer: None,
        });
        self.arm_holt_notice_timer(id, cx);
        cx.notify();
    }

    /// (Re)arm the auto-dismiss timer for a single notice. The task
    /// captures the notice id; on fire it removes that exact entry — other
    /// stacked notices are left alone.
    fn arm_holt_notice_timer(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(notice) = self.holt_notices.iter_mut().find(|n| n.id == id) {
            notice.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(2000))
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(pos) = this.holt_notices.iter().position(|n| n.id == id) {
                        this.holt_notices.remove(pos);
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
    }

    /// Hover state flip for one chip — pauses its timer while hovered
    /// and rearms it once the pointer leaves. `false` is delivered when
    /// the element goes away (including via timer fire); the lookup is
    /// guarded so a missing entry is a no-op.
    fn set_holt_notice_hover(&mut self, id: u64, hovered: bool, cx: &mut Context<Self>) {
        let needs_rearm = if let Some(notice) = self.holt_notices.iter_mut().find(|n| n.id == id) {
            if notice.hovered == hovered {
                return;
            }
            notice.hovered = hovered;
            if hovered {
                // Drop the task to cancel it (no epoch guard needed).
                notice.timer = None;
                false
            } else {
                true
            }
        } else {
            return;
        };
        if needs_rearm {
            self.arm_holt_notice_timer(id, cx);
        }
        cx.notify();
    }

    /// Manual dismiss for one chip (the × button). Removes the entry and
    /// drops its timer; other notices are untouched.
    fn dismiss_holt_notice(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(pos) = self.holt_notices.iter().position(|n| n.id == id) {
            self.holt_notices.remove(pos);
            cx.notify();
        }
    }

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if section != SettingsSection::Providers
            && let Some(page) = self.providers_page.as_ref()
        {
            page.update(cx, |page, cx| page.clear_revealed(cx));
        }
        // Recreate per visit: the page's ListProvideres load re-probes which
        // CLIs are installed, so installing one shows up on the next open.
        if section == SettingsSection::Providers {
            self.providers_page = None;
        }
        // Same freshness for Skills: the page's ListSkills rescan reflects
        // filesystem changes since the last visit without a restart.
        if section == SettingsSection::Skills {
            self.skills_page = None;
        }
        // Same for General: the title-settings read picks up provider key and
        // model changes since the last visit.
        if section == SettingsSection::General {
            self.general_page = None;
        }
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.providers_page.as_ref() {
            page.update(cx, |page, cx| page.clear_revealed(cx));
        }
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        if !matches!(entry, NavEntry::Settings(SettingsSection::Providers))
            && let Some(page) = self.providers_page.as_ref()
        {
            page.update(cx, |page, cx| page.clear_revealed(cx));
        }
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                if section != SettingsSection::Providers
                    && let Some(page) = self.providers_page.as_ref()
                {
                    page.update(cx, |page, cx| page.clear_revealed(cx));
                }
                self.route = Route::Settings(section);
            }
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Providers => {
                if self.providers_page.is_none() {
                    let state = self.state.clone();
                    let page = cx.new(|cx| ProvidersPage::new(state, cx));
                    // Action failures surface as the shell's window-top error
                    // alert, not inside the page.
                    self.providers_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ProvidersPageEvent, cx| {
                            let ProvidersPageEvent::Error(message) = event;
                            this.show_provider_error(message.clone(), cx);
                        },
                    ));
                    self.providers_page = Some(page);
                }
                match &self.providers_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Skills => {
                if self.skills_page.is_none() {
                    let state = self.state.clone();
                    self.skills_page =
                        Some(cx.new(|cx| crate::settings::skills::SkillsPage::new(state, cx)));
                }
                match &self.skills_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::General => {
                if self.general_page.is_none() {
                    let state = self.state.clone();
                    self.general_page =
                        Some(cx.new(|cx| crate::settings::general::GeneralPage::new(state, cx)));
                }
                match &self.general_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.push_holt_notice(HoltNoticeKind::Error, "Engine not connected".into(), cx);
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.push_holt_notice(HoltNoticeKind::Error, format!("{err}").into(), cx);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    /// Ask before archiving: the confirm dialog holds the chat id until the
    /// user commits (Cancel or navigating away drops it).
    pub(super) fn request_archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        self.archive_confirm = Some(chat_id);
        cx.notify();
    }

    fn confirm_archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.archive_confirm = None;
        self.set_chat_archived(chat_id, true, cx);
    }

    /// The Archive session shortcut. With no chat open, or with an already
    /// archived one, it does nothing — the shortcut archives, it never
    /// unarchives.
    fn archive_selected_chat(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self
            .state
            .read(cx)
            .archivable_selected_chat()
            .map(str::to_string)
        else {
            return;
        };
        self.request_archive_chat(chat_id, cx);
    }

    pub(super) fn set_chat_archived(
        &mut self,
        chat_id: String,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
            cx,
        );
        cx.notify();
    }

    /// A jump shortcut: open the sidebar row at `slot`. A slot past the end of
    /// a short list does nothing. Reads the DISPLAYED order — sort and
    /// grouping view options permute the list, and the chip on a row must
    /// name the key that opens it.
    fn jump_to_session(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(chat_id) = self.sidebar_visible_order(cx).into_iter().nth(slot) else {
            return;
        };
        // Same path a click on that row takes.
        self.open_chat(chat_id, cx);
    }

    /// Whether an overlay that owns the keyboard is up — the add-space
    /// palette, the find-file palette, or a composer picker popover (model
    /// selector, traits, repo, branch…). Session-nav shortcuts
    /// (cycle/jump/archive) go quiet underneath one: gpui runs a matched
    /// binding before any `on_key_down`, so an unguarded jump would switch
    /// sessions UNDER the open popover, stranding it over a session the user
    /// never picked.
    pub(super) fn overlay_owns_keyboard(&self, cx: &App) -> bool {
        self.chat_menu.get().is_some()
            || self.add_space.is_some()
            || self.file_lookup.is_some()
            || self.composer.read(cx).pickers().read(cx).is_open()
    }

    /// Track the held modifiers so the sidebar can show its jump hints. Only a
    /// change in visibility repaints — modifier traffic is otherwise constant.
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let mods = &event.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        // No hints while an overlay owns the keyboard — the jumps they
        // advertise are suppressed there.
        let visible = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && jump_hints_visible(&self.settings.keymap, primary, mods.alt, mods.shift);
        self.set_jump_hints(visible, cx);
    }

    pub(super) fn set_jump_hints(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.jump_hints != visible {
            self.jump_hints = visible;
            cx.notify();
        }
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `HOLT_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    fn tween_active(&self, tween: Option<WidthTween>) -> bool {
        tween.is_some_and(|tween| {
            !self.reduced_motion
                && tween.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
        })
    }

    fn active_tween_endpoints(&self, tween: Option<WidthTween>) -> Option<(f32, f32)> {
        tween
            .filter(|transition| {
                !self.reduced_motion
                    && transition.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
            })
            .map(|transition| (transition.from, transition.to))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Right-anchored variant for the changes pane. The outer width follows the
    /// existing shell tween, while descendants retain the larger endpoint's
    /// geometry for that 200ms transition. This mirrors the sidebar's stable
    /// inner/clipped outer behavior without changing the center column's
    /// upstream flex layout.
    fn right_pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        let takeover_width = self
            .active_tween_endpoints(self.right_takeover_content_tween)
            .map(|_| self.eval_tween(self.right_takeover_content_tween, target));
        let content_width =
            right_panel_content_width(target, self.active_tween_endpoints(tween), takeover_width);
        div()
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(content_width))
                    .child(inner),
            )
            .into_any_element()
    }

    /// Floating layers owned by the shell: context menus and edit dialogs.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        overlays.extend(self.render_chat_menu(viewport, window, cx));

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        overlays.extend(self.render_file_draft_overlays(viewport, window, cx));
        overlays.extend(self.render_file_menu_overlay(viewport, window, cx));
        if let Some(overlay) = self.render_file_lookup_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. Its terminals and running programs will also end. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        if let Some(chat_id) = self.archive_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Archive session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will move to Settings \u{2192} Archived. You can unarchive it there anytime."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "archive-chat-cancel")
                                .id("archive-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.archive_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Archive")
                                .id("archive-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.confirm_archive_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("archive-chat-dialog", viewport, card));
        }

        if let Some(error) = self.provider_error.clone() {
            // Top-center alert: danger tint, 2s auto-dismiss (the timer is
            // armed in `show_provider_error`), dedicated close button, and no
            // scrim — the page stays live underneath.
            let card = div()
                .id("provider-error-alert")
                .occlude()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .max_w(px(640.0))
                .pl(px(14.0))
                .pr(px(8.0))
                .py(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.danger.opacity(0.35))
                .bg(theme.surface_dialog)
                .shadow_lg()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.danger_muted)
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(15.0))
                        .flex_none()
                        .text_color(theme.danger),
                )
                .child(error)
                .child(
                    div()
                        .id("provider-error-dismiss")
                        .flex_none()
                        .cursor_pointer()
                        .p(px(4.0))
                        .rounded(px(6.0))
                        .hover(|style| style.bg(crate::theme::ink(0.08)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.provider_error = None;
                            this.provider_error_timer = None;
                            cx.notify();
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .into_any_element();
            overlays.push(popover::top_alert("provider-error-alert", viewport, card));
        }

        if !self.holt_notices.is_empty() {
            // Top-right stacked holt notices. Newest at the bottom of the
            // visual stack (matches the top alert above) so a fresh notice
            // does not push existing ones off-screen; a single fixed slot
            // means the column only grows downward and only ever needs one
            // anchor. Each chip animates in independently on its own id.
            let mut stack = div()
                .id("holt-notice-stack")
                .w(viewport.width)
                .flex()
                .flex_col()
                .items_end()
                .gap(px(8.0))
                .pt(px(14.0))
                .pr(px(14.0));
            for notice in &self.holt_notices {
                let id = notice.id;
                // Outer div satisfies `dialog_in`'s `Styled` bound; the
                // chip itself is a `Stateful<Div>` because of the hover
                // listener and is rendered as `AnyElement`.
                stack = stack.child(motion::dialog_in(
                    ("holt-notice", id),
                    div().child(self.render_holt_notice(notice, &theme, cx)),
                ));
            }
            overlays.push(
                gpui::deferred(
                    gpui::anchored()
                        .position(gpui::point(px(0.0), px(0.0)))
                        .child(stack),
                )
                .priority(3)
                .into_any_element(),
            );
        }

        overlays
    }

    /// Render one stacked top-right notice chip. Color and icon are
    /// driven by `notice.kind` so success / error / etc. read at a glance.
    /// Returns `AnyElement` — the outer caller wraps it in
    /// `motion::dialog_in` for the per-chip entrance animation.
    fn render_holt_notice(
        &self,
        notice: &HoltNotice,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = notice.id;
        let (accent, text) = holt_notice_palette(notice.kind, theme);
        let glyph = holt_notice_icon(notice.kind);
        div()
            .id(("holt-notice-card", id))
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .max_w(px(420.0))
            .pl(px(14.0))
            .pr(px(8.0))
            .py(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(accent.opacity(0.35))
            .bg(theme.surface_dialog)
            .shadow_lg()
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(text)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_holt_notice_hover(id, *hovered, cx);
            }))
            .child(icon(glyph).size(px(15.0)).flex_none().text_color(accent))
            .child(notice.message.clone())
            .child(
                div()
                    .id(("holt-notice-dismiss", id))
                    .flex_none()
                    .cursor_pointer()
                    .p(px(4.0))
                    .rounded(px(6.0))
                    .hover(|style| style.bg(crate::theme::ink(0.08)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dismiss_holt_notice(id, cx);
                    }))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let theme = Theme::of(cx);
        let fade_key = format!("pane-resize-{id}");
        let highlight = motion::hover_blend(
            &fade_key,
            theme.border_strong.opacity(0.0),
            theme.border_strong,
        );
        let clear = highlight.opacity(0.0);
        div()
            .id(id)
            .absolute()
            .top(px(PANE_RESIZE_HITBOX_TOP))
            .bottom_0()
            .w(px(12.0))
            .flex_none()
            .cursor_col_resize()
            .on_hover(motion::hover_listener(fade_key))
            // Codex-style seam feedback: the existing 1px panel border stays
            // visible at rest; hover adds a stronger center highlight that
            // fades back into that border toward both ends.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(6.0))
                    .w(px(1.0))
                    .flex()
                    .flex_col()
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(clear, 0.0),
                        gpui::linear_color_stop(highlight, 1.0),
                    )))
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(highlight, 0.0),
                        gpui::linear_color_stop(clear, 1.0),
                    ))),
            )
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar). Settings never
        // underlaps: pad below the overlaid titlebar.
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let no_project = self.state.read(cx).no_project;

        // Content outlet: selected chat → transcript; nothing selected → a
        // bare canvas (the composer stack carries the affordances); no spaces
        // at all → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            // Re-roll the new-chat mark while the canvas is hidden, so the
            // next bare-canvas visit shows a fresh random shape.
            self.new_chat_mark = random_mark_index();
            self.transcript.clone().into_any_element()
        } else if !has_spaces && !no_project {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a project to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(crate::typography::ui_rems(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A project is a folder on this machine.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a project")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas: the holt mark (mona) over a prompt naming the
            // selected project (user request). The project selectors live
            // above the composer pill (composer.rs renders them via
            // `render_target_selectors`).
            let project = self
                .state
                .read(cx)
                .selected_space_row()
                .map(|space| space.display_name().to_string());
            let prompt: SharedString = match project {
                Some(name) => format!("What should we build in {name}?").into(),
                None => "What should we build?".into(),
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(loaders::holt_mark_loader(
                            "new-chat-mark",
                            theme,
                            56.0,
                            loaders::MARK_SHAPES[self.new_chat_mark],
                            cx.entity_id(),
                            cx,
                        ))
                        .child(
                            div()
                                .mt(px(16.0))
                                .text_size(crate::typography::ui_rems(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(prompt),
                        ),
                ))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. GPUI derives the veil's visibility from the
        // active payload type: an internal drag such as a pane resize must
        // never be able to resurrect stale external-file hover state.
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The dock sits below this entire row; only the composer
                    // and status strip overlap the transcript viewport.
                    let stack_h = self.bottom_stack.get();
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                true,
                                true,
                                div().size_full().child(outlet),
                            )
                            // Fully faded BY the titlebar's bottom edge (the
                            // title text is opaque — overlap read as collision),
                            // ramping in the band just below it.
                            .inset_top(Theme::TITLEBAR_HEIGHT)
                            .band_top(Theme::TRANSCRIPT_FADE_BAND)
                            .band_bottom(bottom_band),
                        )
                        .children(self.render_jump_to_bottom(stack_h, cx))
                },
            )
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts) and composer. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(status)
                    .when(has_spaces, |el| el.child(self.composer.clone()))
            })
            .child(
                div()
                    .invisible()
                    .absolute()
                    .inset_0()
                    .bg(theme.scrim().opacity(0.4 / 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child("Drop images to attach")
                    .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.visible())
                    .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                        let paths = paths.paths().to_vec();
                        this.composer
                            .update(cx, |composer, cx| composer.add_paths(paths, cx));
                        cx.notify();
                    })),
            )
            // The same veil for entries dragged from the File sidebar
            // (ticket 09): an internal drag never triggers the
            // ExternalPaths layer above, so tree drags get their own — the
            // whole conversation column is the drop surface, matching the
            // OS-file behavior. The pill itself carries a tighter highlight
            // (composer.rs); both funnels stage the same path reference.
            .child(
                div()
                    .invisible()
                    .absolute()
                    .inset_0()
                    .bg(theme.scrim().opacity(0.4 / 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child("Drop to attach")
                    .drag_over::<crate::files::tree::TreeEntryDrag>(|style, _, _, _| {
                        style.visible()
                    })
                    .on_drop(cx.listener(
                        |this, entry: &crate::files::tree::TreeEntryDrag, _, cx| {
                            this.composer.update(cx, |composer, cx| {
                                composer.add_paths(
                                    vec![std::path::PathBuf::from(entry.path.as_str())],
                                    cx,
                                );
                            });
                            cx.notify();
                        },
                    )),
            )
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    /// `stack_h` is the measured bottom chrome stack the full-height
    /// transcript scrolls under — the pill anchors just above it (the -14
    /// carries the old status-strip overlap).
    fn render_jump_to_bottom(
        &mut self,
        stack_h: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom(px(stack_h - 14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(self.jump_pill("jump-to-bottom", "jump-pill", self.transcript.clone(), cx))
                .into_any_element(),
        )
    }

    /// The jump pill itself — shared between the conversation overlay and
    /// the subagent pane so both read as one control. `anim_key`/`hover_key`
    /// must be distinct per instance (they key global animation state).
    ///
    /// Glass-forward like the composer pill it floats near: a backdrop blur
    /// under the floating-card tint ([`Theme::glass_overlay`]), hover
    /// brightening via the standard glass wash painted OVER the tint —
    /// mixing the tint TOWARD the wash would thin the pill on hover, the
    /// exact see-through regression the old opaque pill's comment warned
    /// about. Opaque appearances keep the raised-surface treatment
    /// (`frosted` passes through there anyway).
    fn jump_pill(
        &self,
        anim_key: &'static str,
        hover_key: &'static str,
        transcript: Entity<Transcript>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx);
        let glass = theme.is_glass();
        let base = if glass {
            theme.glass_overlay()
        } else {
            motion::hover_blend(hover_key, theme.surface_raised, theme.surface_raised_hover)
        };
        let wash = if glass {
            motion::hover_blend(hover_key, gpui::transparent_black(), theme.glass_hover())
        } else {
            gpui::transparent_black()
        };
        let pill = div()
            .id(anim_key)
            .h(px(30.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .cursor_pointer()
            .bg(base)
            .on_hover(motion::hover_listener(hover_key))
            .on_click(cx.listener(move |_, _, _, cx| {
                transcript.update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
            }))
            .child(
                // The hover wash rides an inner full-height layer so it
                // composites over the tint (a div has one bg).
                div()
                    .h_full()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(11.0))
                    .pr(px(13.0))
                    .bg(wash)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("↓")),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Scroll to bottom")),
                    ),
            );
        // Frost OUTSIDE the entry animation (the composer pill's exact
        // composition): one scene layer — blur, then the pill's quads, then
        // glyphs — so the pill always composes over the transcript content
        // scrolling under it, and never loses its washes to the kind-sorted
        // draw order (frost.rs module docs).
        crate::frost::frosted(15.0, 16.0, motion::dialog_in(anim_key, pill)).into_any_element()
    }

    /// Terminal dock below the conversation and right pane: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its 5px read as
        // dead air between the seam and the tab bar (user report).
        let inner = div()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(panel))
            .child(handle.absolute().top_0().left_0().right_0());

        div()
            .debug_selector(|| "bottom-terminal-dock".into())
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (holt's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(crate::typography::ui_rems(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::gradient_spinner(
                    "sending-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    fn render_gate_card(&mut self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // Backend unreachable: quiet centered copy (holt Gate `Failed`),
        // plus a Retry affordance (the native engine doesn't self-redial).
        let content = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(14.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(error.to_string())),
            )
            .child(
                div()
                    .id("retry-engine")
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.glass_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                    .child(SharedString::from("Retry")),
            );
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed (holt App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in("gate-card-failed", div().child(content))),
            )
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport_width = f32::from(window.viewport_size().width);
        // Appearance actions persist independently of the shell. Mirror the
        // globals before any later debounced settings save can overwrite them.
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        let theme = Theme::of(cx);
        // The shell tone (holt `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Linux CSD: (re-)resolve which caption buttons we draw and on which
        // side — decorations can flip server↔client at runtime and the
        // desktop's button layout is user configuration.
        self.linux_captions = Self::resolve_linux_captions(window, cx);
        if cfg!(target_os = "linux") && self.button_layout_sub.is_none() {
            self.button_layout_sub =
                Some(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        if self.activation_sub.is_none() {
            self.activation_sub = Some(cx.observe_window_activation(
                window,
                |this: &mut Shell, window, cx| {
                    if !window.is_window_active() {
                        this.set_jump_hints(false, cx);
                    }
                },
            ));
        }
        // The quit lifecycle needs the shell's file-draft census (ADR-0020);
        // it is a global created before any window, so attach on first
        // render.
        if !self.lifecycle_attached {
            self.lifecycle_attached = true;
            crate::terminal::lifecycle::attach_shell(cx.entity(), cx);
        }

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat)
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(crate::typography::ui_rems(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_file_tree_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (holt __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            // Cmd+S saves the ACTIVE file tab wherever focus sits (the
            // editor, the tree, or the composer) — a no-op otherwise.
            .on_action(cx.listener(|this, _: &crate::files::SaveFile, _, cx| {
                this.save_active_file(cx);
            }))
            // Cmd+F opens the active file tab's in-file search.
            .on_action(
                cx.listener(|this, _: &crate::files::FindInFile, window, cx| {
                    if let RightSurface::File(id) = this.resolved_right_active(cx)
                        && let Some(space) = this.file_space_key(cx)
                        && let Some(tab) =
                            this.file_state.space(&space).and_then(|tabs| tabs.find(id))
                    {
                        let viewer = tab.viewer.clone();
                        viewer.update(cx, |viewer, cx| {
                            viewer.toggle_search(window, cx);
                        });
                    }
                }),
            )
            // Cmd+Shift+P toggles the active Markdown tab's source/preview
            // mode (ticket 08) — a no-op for every other file tab.
            .on_action(cx.listener(|this, _: &crate::files::TogglePreview, _, cx| {
                if let RightSurface::File(id) = this.resolved_right_active(cx)
                    && let Some(space) = this.file_space_key(cx)
                    && let Some(tab) = this.file_state.space(&space).and_then(|tabs| tabs.find(id))
                {
                    let viewer = tab.viewer.clone();
                    viewer.update(cx, |viewer, cx| {
                        viewer.toggle_preview(cx);
                    });
                }
            }))
            // New session works from anywhere — `open_new_session` routes back
            // to chat itself, so Settings is not a dead spot.
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(cx)))
            // Native Settings menu item and the platform convention (Cmd+, on
            // macOS, Ctrl+, elsewhere) always land on the default section.
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(SettingsSection::General, cx)
            }))
            // Chat-scoped, unlike new-session — `cycle_session` holds the guard
            // and says why.
            .on_action(cx.listener(|this, _: &NextSession, _, cx| this.cycle_session(true, cx)))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| this.cycle_session(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            // Chat-scoped like the panel toggles: Settings has no current
            // session to archive. Quiet under an open popover, like the other
            // session-nav shortcuts.
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if matches!(this.route, Route::Chat) && !this.overlay_owns_keyboard(cx) {
                    this.archive_selected_chat(cx)
                }
            }))
            // A jump routes back to chat itself, so Settings is not a dead
            // spot — the same call a click on that sidebar row makes. But an
            // open picker/palette owns the keyboard: no jumping underneath
            // it. The MODEL menu advertises these same slots on its rows and
            // this matched binding beats its key handler to the dispatch —
            // forward the slot instead of eating it.
            .on_action(cx.listener(|this, jump: &JumpSession, _, cx| {
                let pickers = this.composer.read(cx).pickers().clone();
                let handled = pickers.update(cx, |pickers, cx| pickers.jump_model_slot(jump.0, cx));
                if !handled && !this.overlay_owns_keyboard(cx) {
                    this.jump_to_session(jump.0, cx)
                }
            }))
            .on_modifiers_changed(
                cx.listener(|this, event, _, cx| this.on_modifiers_changed(event, cx)),
            )
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFileLookup, _, cx| {
                this.toggle_file_lookup(cx);
            }));

        let root = match &gate {
            // The auth gates are unreachable on the local-only runtime; treat
            // them as Ready rather than dead-ending the shell.
            GatePhase::Ready => {
                // Focus is a sync signal: on the rising edge of window
                // activation, nudge every open room to verify liveness — a
                // broadcast-deaf socket (accepted writes, runtime pongs,
                // nothing delivered; 2026-08-04 incident) then heals within
                // seconds of the user looking at the app rather than waiting
                // out the background probe cadence.
                let window_active = window.is_window_active();
                if window_active && !self.was_window_active {
                    self.state.update(cx, |s, cx| s.probe_sync(cx));
                }
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `HOLT_OPEN_DIALOG=model` pops the combined
                // provider/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                // Stamped for `right_target` — the expanded changes panel
                // sizes itself to the viewport.
                self.viewport_width = viewport;
                let main_target_width =
                    conversation_width(viewport, self.sidebar_target(), self.right_target(cx));
                let main_transition = self.active_tween_endpoints(self.main_takeover_tween);
                let main_content_width =
                    stable_panel_content_width(main_target_width, main_transition);
                let main_width = (main_content_width - 10.0).max(0.0);
                self.composer.update(cx, |composer, cx| {
                    composer.set_available_width(main_width, cx)
                });
                let stack_h = self.bottom_stack.get();
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx);
                    t.set_bottom_clearance(stack_h, cx);
                });

                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (holt __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                let right_open = on_chat && self.right_pane_open(cx);
                // Takeover mode derives its width from the viewport, so a
                // manual drag handle would fight the expanded target.
                let right_handle = (right_open
                    && !self.right_pane_expanded
                    && !self.tween_active(self.right_tween))
                .then(|| {
                    self.resize_handle(
                        "right-pane-resize",
                        || RightPaneResize,
                        |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                        cx,
                    )
                    // A forgiving transparent hit target centered on the
                    // seam; the panel's 1px border remains the visual divider.
                    .left(px(-6.0))
                });
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // Copied out (not held) — `render_title_bar` needs `cx` mutable.
                let border_color = Theme::of(cx).border;
                // No inset cards (user request): the conversation column sits
                // flush and unbordered, the transcript directly on the frost
                // glass; the changes pane is a flush left-bordered glass panel
                // (built inside `render_right_pane`).
                let main = if main_transition.is_some() {
                    div()
                        .h_full()
                        .w(px(main_content_width))
                        .flex_none()
                        .flex()
                        .child(main)
                        .into_any_element()
                } else {
                    main
                };
                let card: AnyElement = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .child(main)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (holt
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.left(px(-6.0)));
                // Keep the right resize target outside the pane's
                // overflow-hidden width container. This mirrors the sidebar
                // seam and lets the target straddle both adjacent panes.
                let right_seam: AnyElement = if let Some(handle) = right_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                // The far-right File tree column (ADR-0020): its own seam,
                // resize handle, and open/close tween. It renders on chat
                // routes and the space-keyed new-chat canvas alike, and
                // collapses to nothing below its width floor (decision 17).
                let tree_showing = on_chat
                    && (self.file_tree_target(cx) > 0.0 || self.tween_active(self.file_tree_tween));
                let tree_handle = (on_chat
                    && self.file_tree_visible
                    && self.file_tree_available(cx) >= FILE_TREE_MIN
                    && !self.tween_active(self.file_tree_tween))
                .then(|| {
                    self.resize_handle(
                        "file-tree-resize",
                        || FileTreeResize,
                        |shell, _| shell.settings.file_tree_width = FILE_TREE_DEFAULT,
                        cx,
                    )
                    .left(px(-6.0))
                });
                let file_tree: AnyElement = if tree_showing {
                    self.render_file_tree_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let file_tree_seam: AnyElement = if let Some(handle) = tree_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                // The content row spans the FULL window height — the titlebar
                // overlays it (glass, no fill), so the transcript can scroll
                // under the header and fade out at its edge. Columns that
                // must NOT underlap (sidebar content, the changes panel,
                // settings) pad themselves down by the titlebar height.
                let page = div()
                    .size_full()
                    .relative()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(
                                div()
                                    .debug_selector(|| "workspace-content".into())
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .debug_selector(|| "workspace-top".into())
                                            .flex_1()
                                            .min_h_0()
                                            .flex()
                                            .child(card)
                                            .child(right_seam)
                                            .child(right)
                                            .child(file_tree_seam)
                                            .child(file_tree),
                                    )
                                    .when(on_chat, |el| {
                                        el.child(self.render_terminal_container(cx))
                                    }),
                            ),
                    )
                    .child(div().absolute().top_0().left_0().right_0().child(title_bar))
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::Failed(error) => {
                let card = self.render_gate_card(error, cx);
                root.child(card)
            }
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, false, cx.entity_id(), cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, true, cx.entity_id(), cx))
            }
            SplashPhase::Gone => root,
        };

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above the splash and the error gate as well as the full
        // application. Gate pages also need a drag surface because they do
        // not render the unified tabs/settings titlebar — on Windows the
        // native `Drag` control area, on Linux the explicit
        // `start_window_move` strip (the control-area hit-test is inert
        // there); macOS drags gate windows natively.
        let root = if matches!(gate, GatePhase::Ready) || cfg!(target_os = "macos") {
            root
        } else {
            root.child(
                self.titlebar_drag_region(
                    "gate-titlebar-drag",
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(Theme::TITLEBAR_HEIGHT)),
                    cx,
                ),
            )
        };
        root.children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The merge-ownership invariant: a Shell layout save must merge only the
    /// fields the Shell owns, so a stale working copy cannot roll back
    /// another writer's choices (notification toggles, disabled skills).
    #[gpui::test]
    fn schedule_save_merges_only_shell_owned_fields(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            let mut seeded = UiSettings::default();
            seeded.completion_notifications = false;
            seeded.completion_notification_sound = false;
            seeded.disabled_skills = vec!["grill".into()];
            crate::settings::init(seeded, dir.path(), cx);
        });
        let state = cx.new(|_| AppState::new());
        let shell = cx.new(|cx| {
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            )
        });
        shell.update(cx, |shell, cx| {
            shell.settings.sidebar_width = 300.0;
            shell.schedule_save(cx);
        });
        cx.update(|cx| {
            let current = crate::settings::current(cx);
            assert_eq!(current.sidebar_width, 300.0);
            assert!(!current.completion_notifications);
            assert!(!current.completion_notification_sound);
            assert_eq!(current.disabled_skills, ["grill".to_string()]);
            crate::settings::flush(cx);
        });
        let reloaded = UiSettings::load(dir.path());
        assert_eq!(reloaded.sidebar_width, 300.0);
        assert!(!reloaded.completion_notifications);
        assert!(!reloaded.completion_notification_sound);
        assert_eq!(reloaded.disabled_skills, ["grill".to_string()]);
    }

    #[gpui::test]
    fn terminal_hosts_coexist_below_and_beside_the_chat(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_chat = Some("terminal-layout".into());
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "terminal-layout", "deviceId": "test-device", "archived": false,
                    "cwd": "/tmp", "createdAt": "2026-09-07T00:00:00Z"
                }))
                .unwrap(),
            );
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.debug_gate = Some(GatePhase::Ready);
            shell.splash = SplashPhase::Gone;
            shell.route = Route::Chat;
            shell.active_chat = "terminal-layout".into();
            shell
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.toggle_terminal(window, cx);
                shell.toggle_right_pane(cx);
                let bottom = shell.terminal_panel(cx);
                let right = shell.right_terminal_panel(cx);
                assert_ne!(bottom.entity_id(), right.entity_id());
                shell.set_right_active(RightSurface::Terminal(1), cx);
                assert!(shell.terminal_open(cx));
                assert!(shell.right_pane_open(cx));
                shell.toggle_terminal(window, cx);
                assert!(!shell.terminal_open(cx));
                assert!(shell.right_pane_open(cx));
                shell.toggle_terminal(window, cx);
                assert!(shell.terminal_open(cx));
                assert!(shell.right_pane_open(cx));
                assert_eq!(bottom.entity_id(), shell.terminal_panel(cx).entity_id());
                assert_eq!(
                    right.entity_id(),
                    shell.right_terminal_panel(cx).entity_id()
                );
                shell.terminal_tween = None;
                shell.right_tween = None;
            });
        });
        for size in [
            gpui::size(px(1280.0), px(900.0)),
            gpui::size(px(960.0), px(700.0)),
        ] {
            cx.simulate_resize(size);
            cx.run_until_parked();
            let workspace = cx.debug_bounds("workspace-content").unwrap();
            let top = cx.debug_bounds("workspace-top").unwrap();
            let dock = cx.debug_bounds("bottom-terminal-dock").unwrap();
            assert_eq!(dock.left(), workspace.left());
            assert_eq!(dock.right(), workspace.right());
            assert_eq!(dock.top(), top.bottom());
            assert_eq!(dock.bottom(), workspace.bottom());
            assert!(top.size.height > px(0.0));
            assert!(dock.size.height >= px(160.0));
        }
    }

    #[test]
    fn every_default_shortcut_binds_on_this_platform() {
        // `apply_keymap` silently falls back on an unparseable combo, so a
        // default gpui cannot parse would ship as a dead shortcut.
        for id in crate::settings::ShortcutId::ALL {
            let combo = platform_combo(id.default_combo());
            assert!(
                Keystroke::parse(&combo).is_ok(),
                "{} default {combo:?} does not parse",
                id.label()
            );
        }
    }

    #[test]
    fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
        assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
    }

    #[test]
    fn right_panel_content_keeps_the_larger_width_only_during_transition() {
        assert_eq!(right_panel_content_width(520.0, None, None), 520.0);
        assert_eq!(
            right_panel_content_width(0.0, Some((520.0, 0.0)), None),
            520.0
        );
        assert_eq!(
            right_panel_content_width(760.0, Some((520.0, 760.0)), None),
            760.0
        );
        assert_eq!(
            right_panel_content_width(1064.0, Some((520.0, 1064.0)), Some(760.0)),
            760.0
        );

        let conversation = conversation_width(1320.0, 256.0, 520.0);
        let takeover = conversation_width(1320.0, 256.0, 1064.0);
        assert_eq!(conversation, 544.0);
        assert_eq!(takeover, 0.0);
        assert_eq!(
            stable_panel_content_width(takeover, Some((conversation, takeover))),
            conversation
        );
        assert_eq!(
            stable_panel_content_width(conversation, Some((takeover, conversation))),
            conversation
        );
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Providers));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Providers))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Appearance));
        nav.push(NavEntry::Settings(SettingsSection::Appearance));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — holt's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Providers));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Providers))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }
}
