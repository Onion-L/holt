//! Synced entity rows (workspace doc) and local projections.
//!
//! In holt these were synced Postgres rows; in holt they live in the per-org
//! workspace Loro doc (see ARCHITECTURE.md §2.2) with the same field surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{PermissionMode, ProviderId, ReasoningLevel};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// First registration time (holt devices.created_at — the Devices page
    /// "Added …" fragment). Optional so pre-existing docs stay readable.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// App version the device's engine last booted with — fleet staleness at a
    /// glance (Devices page). Optional so pre-existing docs stay readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A synced (device, folder) pair — the unit of organization in the sidebar.
/// Sessions belong to exactly one space; the space fixes their host device and
/// base cwd. Folders need not be git repos: `git_detected` is stamped by the
/// owning device (SpacesSync) and gates branch pickers / the diff sidebar on
/// every device without an RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    /// Owning device — fixed at create, immutable.
    pub device_id: String,
    /// Absolute folder path on the owning device.
    pub path: String,
    /// User rename; absent ⇒ display = basename(path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Owner-stamped: is `path` inside a git work tree?
    #[serde(default)]
    pub git_detected: bool,
    /// Owner-stamped freshness timestamp of the last git check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_checked_at: Option<DateTime<Utc>>,
    /// Owner-stamped when git: canonical checkout identity of the space root
    /// (sha256(deviceId ‖ NUL ‖ git_dir)) — diff grouping key for root sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Space {
    /// Name override, else basename(path), else the path itself.
    /// Lives here (proto) so UI and engine agree on the derivation.
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.as_deref()
            && !name.trim().is_empty()
        {
            return name;
        }
        let trimmed = self.path.trim_end_matches(['/', '\\']);
        trimmed
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatConfig {
    pub provider: ProviderId,
    pub model: String,
    pub reasoning: Option<ReasoningLevel>,
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The chat's permission mode (ADR-0014). Configs stored by the sandbox
    /// era carry this under the `sandbox` key — the alias keeps them
    /// readable — and a config without the field defaults to confirm-changes.
    #[serde(default, alias = "sandbox")]
    pub permission_mode: PermissionMode,
}

/// The built-in Title-task instruction (ADR-0012) — the single source the
/// engine's defaulting and the settings UI's restore-default button share.
/// The 60-character ceiling it names is enforced as `TITLE_CHAR_LIMIT` in
/// `crates/engine` (a string here can't reference a const across crates).
pub const DEFAULT_TITLE_INSTRUCTION: &str = "Write a short, scannable title (one line, at most 60 characters) for a chat that begins with this message. Reply with the title text only.";

fn default_title_instruction() -> String {
    DEFAULT_TITLE_INSTRUCTION.to_string()
}

/// Engine-owned title-task configuration (ADR-0012), persisted per device
/// and read/written only through typed RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleSettings {
    /// Provider-qualified model id (`"openai/gpt-5.4"`); `None` disables
    /// automatic titles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// The fixed instruction sent alongside the first user prompt.
    #[serde(default = "default_title_instruction")]
    pub instruction: String,
}

impl Default for TitleSettings {
    fn default() -> Self {
        Self {
            model_id: None,
            instruction: default_title_instruction(),
        }
    }
}

/// Title settings plus the engine's live validation view of them — the
/// reply shape of both the read and the save RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleSettingsState {
    pub settings: TitleSettings,
    /// A visible validation warning (missing provider credentials). A
    /// warning never blocks normal chat Turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Immutable-at-run-start repository context owned by one conversation.
///
/// This is deliberately separate from the live checkout snapshot: another
/// chat may change the branch at the same checkout without changing which
/// branch this conversation belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSourceContext {
    pub checkout_id: String,
    pub repo_root: String,
    pub cwd: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub observed_at: DateTime<Utc>,
}

/// Who owns a chat's title (ADR-0012). `Automatic` titles (the first-line
/// fallback or the one-shot Title task's result) may still be replaced;
/// `UserManual` titles are locked — any rename mutation switches to this
/// state, even when the text is unchanged. Rows that predate this field
/// load as `UserManual` so existing names are never auto-renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TitleSource {
    Automatic,
    #[default]
    UserManual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    /// Owning (host) device.
    pub device_id: String,
    pub title: Option<String>,
    /// Ownership of `title`; see `TitleSource`.
    #[serde(default)]
    pub title_source: TitleSource,
    /// Set once the chat's one-shot automatic Title task has started. A
    /// restarted engine never retries a started task, so this persists with
    /// the chat.
    #[serde(default)]
    pub title_task_started: bool,
    pub archived: bool,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Canonical id of the repo checkout/worktree this chat operates in.
    pub checkout_id: Option<String>,
    /// Repository identity captured for this conversation immediately before
    /// its provider run. Unlike `branch`, this is never inferred from another
    /// chat sharing the same checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<ConversationSourceContext>,
    pub config: Option<ChatConfig>,
    pub last_message_preview: Option<String>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// The space this chat belongs to. Invariant: `Some` for every UI-created
    /// chat; rows with a missing/dangling space id are not rendered (the host
    /// device's repair sweep deletes its own danglers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// Synced LWW seen marker — compared against `last_message_at` to derive
    /// the "completed (finished but unseen)" indicator. Reading a chat on any
    /// device clears the badge everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Which sync room generation serves this chat (docs/chat2-sync.md M2):
    /// `None`/1 = legacy s2 loro room, 2 = chat2 dumb relay. The HOST flips
    /// this in the same breath as seeding the chat2 checkpoint; every device
    /// dials the room the registry names. Per-chat and instantly revertible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_gen: Option<u32>,
    /// The overflow fallback (ADR-0011): the last Turn ended on a context
    /// overflow, so the next Turn compacts unconditionally before its
    /// first request. Consumed by that Turn.
    #[serde(default)]
    pub compact_before_next_turn: bool,
}

impl Chat {
    /// True when this chat syncs over the chat2 dumb relay.
    pub fn on_chat2(&self) -> bool {
        self.room_gen.unwrap_or(1) >= 2
    }
}

impl Chat {
    /// True when the chat has activity the user hasn't seen on any device.
    pub fn unseen(&self) -> bool {
        match (self.last_message_at, self.last_seen_at) {
            (Some(msg), Some(seen)) => msg > seen,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

/// Display status for a chat row/tab: the four user-facing states plus a
/// distinct Errored. Derived — never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatIndicator {
    Working,
    AwaitingInput,
    Errored,
    /// Finished running (or errored out) but not seen yet on any device.
    Completed,
    Idle,
}

/// Derive the display status. `live` must already be staleness-gated by the
/// caller (the UI's 45s window) — pass `None` for a stale/absent session row.
pub fn chat_indicator(chat: &Chat, live: Option<&Session>) -> ChatIndicator {
    match live.map(|s| s.status) {
        Some(SessionStatus::Working) | Some(SessionStatus::Compacting) => ChatIndicator::Working,
        Some(SessionStatus::AwaitingInput) => ChatIndicator::AwaitingInput,
        Some(SessionStatus::Errored) if chat.unseen() => ChatIndicator::Errored,
        _ if chat.unseen() => ChatIndicator::Completed,
        _ => ChatIndicator::Idle,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Idle,
    Working,
    AwaitingInput,
    Errored,
    /// A manual `/compact` is running (ADR-0011): the summary request is
    /// in flight. Displayed like `Working` with the same interrupt
    /// affordance; never set by the automatic in-Turn compaction.
    Compacting,
}

/// Live run status for a chat — drives the Working indicator and sidebar status dots.
/// Staleness-checked client-side against `updated_at` so a crashed backend never shows
/// an eternal "Working".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub chat_id: String,
    pub device_id: String,
    pub status: SessionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub path: String,
    pub name: String,
    pub default_branch: Option<String>,
}

/// One row of `ListRefs`: a branch plus its checkout state — whether it is
/// the repo's current (main-checkout) branch and whether it is materialized
/// as a linked worktree. Drives the composer's ref picker (`current` /
/// `worktree` tags) and the checkout-kind selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRef {
    pub name: String,
    /// Checked out in the repo's MAIN folder right now.
    #[serde(default)]
    pub current: bool,
    /// Path of the linked worktree this branch is checked out in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

/// The first line of a `SwitchRef`/`CreateBranch` refusal error whose dirty
/// tree would be overwritten (ADR-0007): the engine formats it followed by
/// one blocking file path per line, and the UI parses it back to raise the
/// inform-only switch dialog. The error channel is stringly by design; this
/// shared constant is its one definition, with tests pinning the shape on
/// both sides.
pub const SWITCH_REFUSAL_MARKER: &str =
    "switch refused: uncommitted changes would be overwritten by checkout:";

/// Public Git reference attached to a commit in the history graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GitHistoryRefKind {
    Branch,
    Remote,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHistoryRef {
    pub kind: GitHistoryRefKind,
    pub label: String,
}

/// One topologically ordered row in the repository history graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHistoryCommit {
    pub sha: String,
    pub parent_shas: Vec<String>,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    #[serde(default)]
    pub refs: Vec<GitHistoryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHistoryPage {
    pub commits: Vec<GitHistoryCommit>,
    pub head_sha: Option<String>,
    pub next_cursor: Option<usize>,
    pub total_count: Option<usize>,
    /// Number of commits reachable from the active checkout's HEAD.
    #[serde(default)]
    pub head_commit_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub repo_path: String,
    pub path: String,
    pub branch: String,
    /// Generated worktree folder name (`holt/<name>` is its branch).
    #[serde(default)]
    pub name: String,
    /// Canonical checkout identity (device-scoped hash of the git dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderListing {
    pub path: String,
    pub entries: Vec<FolderEntry>,
    /// True when the listing hit the entry cap.
    #[serde(default)]
    pub truncated: bool,
}

/// A browse root beyond home: a mounted drive/volume (or the system root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveEntry {
    /// Display name (volume label / mount folder name; "System" for `/`).
    pub name: String,
    /// Absolute mount point.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveListing {
    pub drives: Vec<DriveEntry>,
}

/// A workspace-relative file or directory returned by `SearchFiles`.
/// Contents deliberately never cross this boundary: mentioning a path leaves
/// the provider to read it through its normal workspace tools when needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchMatch {
    pub path: String,
    pub is_dir: bool,
}

/// What one File-sidebar entry is, after the engine resolved its symlinks.
/// Directory/File describe plain entries; the symlink variants carry whether
/// the entry may be expanded/read inside the sidebar (`SymlinkInside`) or must
/// stay a dead-end row with an external-open affordance (`SymlinkOutside`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceEntryKind {
    Directory,
    File,
    /// A symlink whose resolved target stays inside the tree root.
    /// `targetIsDir` drives the disclosure affordance; `resolvedPath` is the
    /// canonical target the engine reads through (alias-aware tab identity).
    SymlinkInside {
        target_is_dir: bool,
        resolved_path: String,
    },
    /// A symlink whose resolved target left the tree root — shown, never
    /// traversed, opened, or edited by the sidebar.
    SymlinkOutside {
        target_is_dir: bool,
    },
    /// A symlink whose target does not exist.
    SymlinkBroken,
}

/// One row of a `ListWorkspaceEntries` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEntry {
    pub name: String,
    /// Absolute path of the entry itself (symlinks not resolved).
    pub path: String,
    pub kind: WorkspaceEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One directory level of a Space's working directory (File sidebar).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListing {
    /// The canonical directory that was listed.
    pub path: String,
    pub entries: Vec<WorkspaceEntry>,
    /// True when the listing hit the entry cap.
    #[serde(default)]
    pub truncated: bool,
}

/// The line-ending shape a text file was read with. Saving must reproduce
/// what the file had; `Mixed` is reported so the editor can treat the file
/// conservatively instead of silently normalizing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceLineEndings {
    Lf,
    Crlf,
    Mixed,
    /// No line terminator at all (empty or single-line files).
    None,
}

/// One coalesced `WatchWorkspaceEntries` frame: the absolute paths that
/// changed under the watched root since the last frame (creations,
/// modifications, renames, deletions — the UI decides what to re-read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWatchFrame {
    pub paths: Vec<String>,
}

/// The `SaveWorkspaceFile` reply. A version conflict is a REPLY, not an RPC
/// error: the UI keeps the draft and offers the resolution workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileSave {
    pub status: WorkspaceSaveStatus,
    /// The new disk version token after a successful save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The disk version that replaced the expected one on a conflict — the
    /// baseline for a fresh decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceSaveStatus {
    Saved,
    /// The disk version moved since the read the save was based on. Nothing
    /// was written; the draft is untouched.
    VersionConflict,
}

/// The `ReadWorkspaceFile` reply: either editable UTF-8 `text` plus the
/// source facts a later save must preserve, or a typed reason the file
/// cannot be opened in the editor (with an external-open affordance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileRead {
    /// The canonical path the engine read (a symlink's target when the read
    /// went through an inside-root alias).
    pub path: String,
    /// Disk version token at read time (size + mtime). Opaque to the UI;
    /// later save requests echo it back for conflict detection.
    pub version: String,
    /// Byte length on disk.
    pub bytes: u64,
    /// The file's own bytes started with a UTF-8 BOM.
    #[serde(default)]
    pub bom: bool,
    pub line_endings: WorkspaceLineEndings,
    /// `None` with `unsupportedReason` set when the file cannot be edited:
    /// over the size limit, not valid UTF-8, or binary content. Never a
    /// lossy decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
}

/// Which standard skill root an entry was discovered in (ADR-0005): the
/// project root at the chat's cwd wins over the personal home root, which
/// wins over holt's own data-dir root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillRoot {
    /// `.agents/skills` at the chat's working directory.
    Project,
    /// `~/.agents/skills`.
    Personal,
    /// `~/.holt/skills` (the engine data dir).
    Holt,
}

/// An invocable catalog entry: valid, unshadowed, name-addressable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Absolute path of the `SKILL.md` — where the read tool finds it and
    /// what invocation chips point at.
    pub file: String,
    pub root: SkillRoot,
    /// The skill opted out of model-visible listings; it stays invocable
    /// through `/skill` only (ADR-0006).
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// A valid skill that lost a name collision to a nearer root: reported so
/// precedence surprises are explainable, never offered for invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowedSkillEntry {
    pub name: String,
    pub file: String,
    pub root: SkillRoot,
    /// The root whose same-named skill won.
    pub shadowed_by: SkillRoot,
}

/// A load problem the loader reported: an invalid skill (name ≠ directory,
/// missing/oversized description, …) or a root traversal fault, with the
/// loader's own diagnostic message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidSkillEntry {
    /// The path the diagnostic names — the `SKILL.md`, or the directory for
    /// traversal faults.
    pub file: String,
    pub root: SkillRoot,
    /// Skill name when the entry parsed far enough to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
}

/// The `ListSkills` reply: everything the `/` menu, the Settings page, and
/// the run loop consume, from one fresh catalog scan (ADR-0005 — no cache).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillListing {
    pub skills: Vec<SkillEntry>,
    pub shadowed: Vec<ShadowedSkillEntry>,
    pub invalid: Vec<InvalidSkillEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileSummary {
    pub path: String,
    /// Previous path for renames/copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    #[serde(default)]
    pub binary: bool,
}

/// Working-tree diff for a checkout — latest-only sidecar, 3MiB patch cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiff {
    pub checkout_id: String,
    pub device_id: String,
    pub cwd: String,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    /// True when the patch was truncated at the byte cap ("Partial snapshot").
    pub truncated: bool,
    /// Content key of this capture: the SHA-256 hex of
    /// `head_sha ‖ NUL ‖ mode ‖ baseRef ‖ patch_bytes`, where `mode` is the
    /// capture's scope wire value ("workingTree", "branch", "turn", or
    /// "commit"), `baseRef` is the scope's base ref or the empty string,
    /// and `patch_bytes` is the (possibly truncated) patch text. HEAD and
    /// the scope fold in, so a commit that leaves the patch text identical
    /// still re-keys the capture.
    pub checksum: String,
    pub updated_at: DateTime<Utc>,
}

/// Provider-neutral lifecycle state for a code change request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeRequestState {
    Open,
    Closed,
    Merged,
}

/// Compact provider-neutral change request metadata for checkout surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRequestSummary {
    pub provider: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: ChangeRequestState,
    pub base_ref: String,
    pub head_ref: String,
}

/// Latest successful change request resolution for one checkout and branch.
///
/// `change_request: None` is an authoritative successful lookup with no match;
/// resolution failures must retain the previous successful snapshot instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutChangeRequestStatus {
    pub checkout_id: String,
    pub device_id: String,
    pub cwd: String,
    pub branch: String,
    pub change_request: Option<ChangeRequestSummary>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCheckoutFileDiffTextRequest {
    pub checkout_id: String,
    pub cwd: String,
    pub path: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Pinned commit for History's per-commit diff scope. When present, the
    /// source pair is read from the commit parent and this commit, never from
    /// the live working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    pub diff_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutFileDiffText {
    pub diff_checksum: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_content_hash: Option<String>,
    pub binary: bool,
    pub truncated: bool,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum AuthState {
    SignedOut,
    NeedsOrganization {
        user: UserProfile,
    },
    #[serde(rename_all = "camelCase")]
    SignedIn {
        user: UserProfile,
        org_id: Option<String>,
    },
}

/// An open PTY session on the owning device (`OpenTerminal` reply).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSession {
    pub id: String,
    pub cwd: String,
    /// Shell basename (`zsh`, `bash`, …) for the tab label.
    pub shell: String,
}

/// One `SubscribeTerminal` stream item. `seq` is a per-terminal monotonic counter
/// used for replay resumption (`afterSeq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TerminalEvent {
    /// Replay was evicted. The following bytes are only a retained tail;
    /// the UI must disclose that the terminal screen is incomplete.
    Gap { seq: u64 },
    /// Output chunk; `data` is base64 (PTY output is raw bytes, not valid UTF-8).
    Data { seq: u64, data: String },
    #[serde(rename_all = "camelCase")]
    Exit {
        seq: u64,
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
}

/// Live edge-connectivity posture (the `WatchConnectivity` stream): the truth
/// the connection pill, composer honesty, and queued-send badges render.
/// Derived engine-side from the registry room's reconnect state, the OS
/// network-path monitor, and each open chat room's stats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connectivity {
    pub state: ConnectivityState,
    /// Epoch ms of the next scheduled registry dial while reconnecting
    /// (0 = none pending / dialing right now). The countdown renders
    /// client-side from this.
    #[serde(default)]
    pub retry_at_ms: i64,
    /// The failure that started the current outage — sticky through the next
    /// attempt (no flicker back to a bare "connecting…"), cleared on rejoin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    /// Per-OPEN-chat room state; a chat absent here is unknown (consumers
    /// fall back to the global state).
    #[serde(default)]
    pub chats: Vec<ChatConnectivity>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectivityState {
    /// No edge transports on this profile (local scope) — hide the pill.
    #[default]
    Disabled,
    /// The OS reports no network path.
    Offline,
    /// Edge expected but the registry room is down (dialing/backing off).
    Reconnecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatConnectivity {
    pub chat_id: String,
    pub connected: bool,
    /// Local update batches not yet acked by the chat's edge room.
    #[serde(default)]
    pub pending_pushes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn skill_listing_round_trips_as_camel_case() {
        let listing = SkillListing {
            skills: vec![SkillEntry {
                name: "grill".into(),
                description: "Relentlessly interview a plan.".into(),
                file: "/home/u/.agents/skills/grill/SKILL.md".into(),
                root: SkillRoot::Personal,
                disable_model_invocation: true,
            }],
            shadowed: vec![ShadowedSkillEntry {
                name: "grill".into(),
                file: "/repo/.agents/skills/grill/SKILL.md".into(),
                root: SkillRoot::Holt,
                shadowed_by: SkillRoot::Project,
            }],
            invalid: vec![InvalidSkillEntry {
                file: "/repo/.agents/skills/draft/SKILL.md".into(),
                root: SkillRoot::Project,
                name: None,
                message: "description is required".into(),
            }],
        };
        let value = serde_json::to_value(&listing).unwrap();
        assert_eq!(value["skills"][0]["root"], "personal");
        assert_eq!(value["skills"][0]["disableModelInvocation"], true);
        assert_eq!(value["shadowed"][0]["shadowedBy"], "project");
        assert_eq!(
            value["invalid"][0]["file"],
            "/repo/.agents/skills/draft/SKILL.md"
        );
        assert_eq!(
            serde_json::from_value::<SkillListing>(value).unwrap(),
            listing
        );
    }

    #[test]
    fn checkout_change_request_status_round_trips_all_states_as_camel_case() {
        for (state, encoded_state) in [
            (ChangeRequestState::Open, "open"),
            (ChangeRequestState::Closed, "closed"),
            (ChangeRequestState::Merged, "merged"),
        ] {
            let status = CheckoutChangeRequestStatus {
                checkout_id: "checkout-1".into(),
                device_id: "device-1".into(),
                cwd: "/repo".into(),
                branch: "feature/change".into(),
                change_request: Some(ChangeRequestSummary {
                    provider: "github".into(),
                    number: 90,
                    title: "Model checkout change request status".into(),
                    url: "https://github.com/acme/holt/pull/90".into(),
                    state,
                    base_ref: "main".into(),
                    head_ref: "feature/change".into(),
                }),
                updated_at: Utc.with_ymd_and_hms(2026, 8, 15, 12, 30, 0).unwrap(),
            };

            let value = serde_json::to_value(&status).unwrap();
            assert_eq!(value["checkoutId"], "checkout-1");
            assert_eq!(value["deviceId"], "device-1");
            assert_eq!(value["changeRequest"]["state"], encoded_state);
            assert_eq!(value["changeRequest"]["baseRef"], "main");
            assert_eq!(value["changeRequest"]["headRef"], "feature/change");
            assert_eq!(
                serde_json::from_value::<CheckoutChangeRequestStatus>(value).unwrap(),
                status
            );
        }
    }

    #[test]
    fn checkout_file_diff_text_contract_is_camel_case() {
        let request = GetCheckoutFileDiffTextRequest {
            checkout_id: "checkout".into(),
            cwd: "/repo".into(),
            path: "src/lib.rs".into(),
            mode: "branch".into(),
            base_ref: Some("main".into()),
            chat_id: None,
            commit_sha: Some("deadbeef".into()),
            diff_checksum: "abc".into(),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["checkoutId"], "checkout");
        assert_eq!(value["diffChecksum"], "abc");
        assert_eq!(value["commitSha"], "deadbeef");
        assert_eq!(
            serde_json::from_value::<GetCheckoutFileDiffTextRequest>(value).unwrap(),
            request
        );
    }
}
