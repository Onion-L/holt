//! Agent-side wire types: provider/model run configuration and streaming events.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// A concrete provider under a [`Provider`] row: the unit every per-provider
/// RPC (`SaveProviderKey`, `ListModels`, `AddProviderModel`, run requests)
/// addresses by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderVariant {
    pub id: ProviderId,
    pub name: String,
    pub configured: bool,
}

/// One provider-catalog row. Standalone providers carry a single variant that
/// repeats the row `id`; organizations group sibling providers (`minimax` +
/// `minimax-cn`) so the settings page shows one card per organization and the
/// expanded panel picks a variant. The row `id` is the organization key —
/// never a runnable provider by itself when `variants` is longer than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: ProviderId,
    pub name: String,
    pub abbreviation: String,
    /// True when any variant has a stored key.
    pub configured: bool,
    pub variants: Vec<ProviderVariant>,
}

impl Provider {
    /// Flatten the row into concrete per-variant descriptors — the shape the
    /// composer and model picker operate on (`provider/model` addressing).
    /// Flattened rows carry no further variants.
    pub fn concrete_providers(&self) -> Vec<Provider> {
        self.variants
            .iter()
            .map(|variant| Provider {
                id: variant.id.clone(),
                name: variant.name.clone(),
                abbreviation: self.abbreviation.clone(),
                configured: variant.configured,
                variants: Vec::new(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// xhigh + provider-specific setting.
    Ultracode,
    /// Prompt-prefix driven (Claude).
    Ultrathink,
}

/// The chat's permission mode (ADR-0014): which gatekeeper each mutating
/// tool call (write, edit, bash) meets before it executes. Reads and content
/// search are never gated, and there is no read-only tier.
///
/// Stored values from the dormant sandbox era remap on read —
/// `workspace-write`/`read-only` → `confirm-changes`,
/// `danger-full-access` → `full-access` — and any other value falls back to
/// `confirm-changes` (the first-launch default) rather than failing startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    ConfirmChanges,
    AutoReview,
    FullAccess,
}

impl<'de> Deserialize<'de> for PermissionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = PermissionMode;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a permission mode")
            }
            fn visit_str<E>(self, value: &str) -> Result<PermissionMode, E> {
                Ok(match value {
                    "confirm-changes" | "workspace-write" | "read-only" => {
                        PermissionMode::ConfirmChanges
                    }
                    "auto-review" => PermissionMode::AutoReview,
                    "full-access" | "danger-full-access" => PermissionMode::FullAccess,
                    _ => PermissionMode::ConfirmChanges,
                })
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    /// Steer delivered at the next step boundary within the live turn.
    StepBoundary,
    /// Steer delivered only between turns.
    TurnBoundary,
}

/// A user verdict on a pending Approval (ADR-0014), submitted through the
/// `ResolveApproval` RPC: allow this call, always-allow its kind for the
/// rest of the app session (bash by command prefix, write/edit by exact
/// path — chat-scoped, in-memory), or deny it — optionally with a written
/// note whose text becomes the denial reason the model reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ApprovalVerdict {
    Allow,
    AlwaysAllow,
    #[serde(rename_all = "camelCase")]
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageCapability {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub provider: ProviderId,
    pub label: String,
    /// Short tagline rendered under the name in the model picker (11px muted),
    /// mirroring the Electron app's `ModelInfo.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning: Option<ReasoningLevel>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
    /// True when this row is a user-added custom model (settings-page
    /// deletable); builtin catalog rows are not.
    #[serde(default)]
    pub custom: bool,
    /// Provider-declared context window in tokens — the denominator a client
    /// divides the latest request's input by. `None` means "unknown window":
    /// a custom row (the engine only knows a template guess, which must not be
    /// presented as fact) or a host that predates the field. The key stays on
    /// the wire so a custom row reads as an explicit null rather than an
    /// ambiguous missing key.
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub image_capability: ImageCapability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub prompt: String,
    /// Concrete provider/model pair resolved by the composer before send.
    pub provider: ProviderId,
    pub model: String,
    pub reasoning: Option<ReasoningLevel>,
    /// Provider-specific option selections (option id -> choice id), JSON round-tripped.
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub cwd: String,
    /// Advisory/vestigial (ADR-0014): the engine preserves the chat's STORED
    /// mode — a switch lands through the mode RPC and takes effect from the
    /// next Turn — so this field no longer moves a chat's mode. It stays on
    /// the wire for queued and legacy commands, decoding through the
    /// `sandbox` alias (a pre-permission-modes host required that key, so a
    /// command from a new peer is skipped by the doc store's skip-not-fail
    /// drain); absent payloads default to confirm-changes.
    #[serde(default, alias = "sandbox")]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub auto_approve: bool,
    /// Absolute paths of image attachments already staged on the run device
    /// (composer uploads: UploadChunk/UploadCommit → durable path). The same
    /// paths also ride the prompt text as `Attached images (local files …)`
    /// refs (holt's `withAttachments` transport — that's what persists in the
    /// doc); this field additionally lets a provider inline the bytes as image
    /// content blocks. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Host-side isolated-worktree creation (see [`WorktreeSpec`]): when set,
    /// the HOST materializes the worktree at command-drain time and runs there
    /// instead of `cwd`. Additive + serde-defaulted for wire compat — an old
    /// host ignores it and runs in `cwd` (the repo's main checkout).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
}

/// What a queued item executes as (spec: message queue and Steer, ticket
/// 04): an ordinary message or a skill invocation starts its own Turn; a
/// manual Compaction occupies the same execution channel without becoming
/// a Turn (ADR-0011). Serde-defaulted so queue files written before typed
/// commands decode as ordinary messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingKind {
    #[default]
    Ordinary,
    Skill,
    Compact,
}

/// An accepted queue item waiting for the execution channel. For
/// `Ordinary` items `request.prompt` is the message body; for `Skill` and
/// `Compact` it is unused — the engine builds the model-visible content at
/// execution admission from `skill_name` / `extra_instructions` (a skill's
/// body is resolved fresh from the filesystem then, never frozen at
/// submission) or from the compaction summary request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMessage {
    pub message_id: String,
    pub request: RunRequest,
    #[serde(default)]
    pub kind: PendingKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_instructions: Option<String>,
    pub submitted_at: i64,
    pub error: Option<String>,
}

/// Authoritative per-chat queue snapshot, independent of the Transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageQueue {
    pub pending: Vec<PendingMessage>,
    pub paused: bool,
    pub active_message_id: Option<String>,
    pub error: Option<String>,
}

/// One `WatchChatUsage` frame: the chat's whole-ledger token totals plus the
/// occupancy of the request it would run next. Every field defaults, so a
/// frame from a peer that predates one of them still decodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsage {
    /// The gross token count over every record: input, output, and both cache
    /// fields of every model call the chat caused.
    #[serde(default)]
    pub gross: u64,
    /// The same records, keyed by their source — the `kind` a usage record
    /// carries (`turn`, `subagent`, `compaction`, `auto-review`, `title`).
    /// Cache tokens are listed apart from the request and reply counts, and
    /// an unknown key is data rather than a decode failure: a newer engine's
    /// new source still reaches a client that has never heard of it.
    #[serde(default)]
    pub by_kind: BTreeMap<String, ChatUsageTokens>,
    /// How many records the totals were summed from.
    #[serde(default)]
    pub record_count: u64,
    #[serde(default)]
    pub occupancy: ChatOccupancy,
}

/// One source's summed token fields, as the frame's `byKind` breakdown
/// reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsageTokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// How full the next request can be expected to leave the window the chat
/// runs against — derived at read time, never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatOccupancy {
    /// The latest main-run provider report's request input, both cache fields
    /// included — or, until this process has seen a report, the History-based
    /// estimate the `estimated` flag declares as such.
    #[serde(default)]
    pub tokens: u64,
    /// The context window of the model the chat runs next (its queue head
    /// while one waits, else its selection). `None` means the window is
    /// unknown — a custom model, whose window the engine deliberately
    /// withholds — and the client shows absolute tokens instead of a
    /// percentage.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Whether `tokens` is the estimator's number rather than a provider's
    /// report.
    #[serde(default)]
    pub estimated: bool,
}

/// One `UsageStats` reply: the device-level usage aggregate behind the
/// Usage overview — every surviving Usage record on the device (live
/// chats' ledgers plus the Usage archive, all five source kinds) merged
/// into one read-only answer. Every field defaults, so a reply from an
/// engine that predates one of them still decodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatsReply {
    /// Chats with at least one record inside the range, deduplicated —
    /// live ledgers attribute by file name, archive rows by the chat id
    /// they were restamped with.
    #[serde(default)]
    pub chat_count: u64,
    /// The range the aggregate covers — the request's `days`, echoed so a
    /// client can tell a stale reply from the range it switched to.
    #[serde(default)]
    pub days: u32,
    /// The six range metrics.
    #[serde(default)]
    pub totals: UsageStatsTotals,
    /// One series per (provider, model), sorted by total descending. Each
    /// series covers exactly the `days` range, oldest first, zero-filled.
    #[serde(default)]
    pub models: Vec<UsageModelSeries>,
    /// The By-model breakdown rows, sorted by total descending.
    #[serde(default)]
    pub by_model: Vec<UsageModelBreakdown>,
    /// The By-project breakdown groups, sorted by total descending. A
    /// project is a chat's working directory, full path; records that
    /// resolve to no directory group under `path: null` ("Deleted chats").
    #[serde(default)]
    pub by_project: Vec<UsageProjectGroup>,
    /// Exactly 365 local-timezone day buckets ending today — the Activity
    /// heatmap's data, independent of `days`. Oldest first, zero-filled.
    #[serde(default)]
    pub heatmap: Vec<UsageStatsDay>,
}

/// The six range metrics: the four headline token sums, the cache hit
/// rate, and the active-days count — all over the selected range only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatsTotals {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// `cacheRead / (cacheRead + input)` — the share of prompt tokens
    /// served from cache; cache writes stay out of the denominator. `None`
    /// when no record in range carried prompt tokens.
    #[serde(default)]
    pub cache_hit: Option<f64>,
    /// How many days in the range carried at least one record.
    #[serde(default)]
    pub active_days: u32,
}

/// One local-timezone day bucket: a calendar date and the gross tokens
/// (input, output, and both cache fields) recorded on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatsDay {
    /// Local calendar day, `YYYY-MM-DD`.
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub tokens: u64,
}

/// One (provider, model) key's daily token series — the same model name
/// under two providers is two independent series.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelSeries {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    /// One bucket per day of the range, oldest first, zero-filled.
    #[serde(default)]
    pub days: Vec<UsageStatsDay>,
}

/// One By-model breakdown row. Total is the four token fields summed;
/// there is deliberately no cache-write column beyond `cacheWrite` in the
/// totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelBreakdown {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub total: u64,
}

/// One By-project group: a working directory's records, or — with
/// `path: null` — the "Deleted chats" group of records whose chat resolves
/// to no working directory (archive rows of deleted chats, cwd-less chat
/// rows), which carries one per-chat row each.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageProjectGroup {
    /// The project's working directory, full path — full, so same-named
    /// basenames never collide. `None` marks the "Deleted chats" group.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub total: u64,
    /// The "Deleted chats" group's per-chat rows, keyed by chat id (the
    /// title died with the chat). Empty on project groups.
    #[serde(default)]
    pub chats: Vec<UsageChatBreakdown>,
}

/// One chat's row inside the "Deleted chats" group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageChatBreakdown {
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub total: u64,
}

/// Isolated-worktree directive riding [`RunRequest`]. The worktree is created
/// by the HOST while draining the queued Run — not by the sender over a
/// blocking CreateWorktree RPC — so the send path stays durable: a lost relay
/// frame can't wedge the composer on "Sending…" while the session runs anyway
/// (2026-08-18 user report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSpec {
    /// The repo whose worktree to create (the space's folder on the host).
    pub repo_path: String,
    /// Base ref the fresh `holt/<name>` branch is created off.
    pub base: String,
}

/// The session-scoped singleton id for the live plan/todo chip. ACP plan
/// updates carry no wire id; adapters emit every update under this one id so
/// the fold refreshes the same chip in place. Consumers that de-duplicate
/// tool ids across segment boundaries (the engine's stale-echo filter) must
/// EXEMPT this id — it legitimately reappears in every segment for the whole
/// life of a run.
pub const LIVE_PLAN_TOOL_ID: &str = "acp-plan";

/// A decoded tool invocation, reduced to the fields each kind renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolCall {
    Exec {
        command: String,
    },
    ReadFile {
        path: String,
    },
    #[serde(rename_all = "camelCase")]
    ReadChat {
        chat_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    WriteFile {
        path: String,
        /// Full content; STRIPPED by the render-parts policy before entering the doc.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    EditFile {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
    },
    ApplyPatch {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// One directory's entries — the `ls` chip.
    ListDir {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    WebFetch {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    Mcp {
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
}

impl ToolCall {
    /// A subagent SPAWN call — the `Agent[: <description>]` naming convention
    /// every driver decodes its spawn tool into (claude/codex `Task`, cursor
    /// `task`, grok `spawn_subagent`, opencode `task`). This is the single
    /// genus gate for subagent binding: tagged subagent traffic may only ever
    /// stamp a ref/status onto a spawn call, so a driver keying bug can never
    /// turn an ordinary Run/Read chip into a spawn chip (2026-08-20: claude's
    /// background-shell `task_notification` did exactly that — the chip
    /// linked to a never-created doc and opened an empty panel).
    pub fn is_subagent_spawn(&self) -> bool {
        let name = match self {
            ToolCall::Unknown { name, .. } => name,
            ToolCall::Mcp { tool, .. } => tool,
            _ => return false,
        };
        name == "Agent" || name.starts_with("Agent: ")
    }

    /// The model a subagent SPAWN was given, when the spawn named one.
    ///
    /// Read off the spawn's own input rather than the session's picked model:
    /// a spawn may override it per child (claude's `Agent` takes `model`, grok
    /// `spawn_subagent` a `model_id`), and two chips spawned in one turn can
    /// legitimately name different models. `None` means the spawn didn't say —
    /// the child inherits the parent's model, which the chip already implies,
    /// so nothing is rendered rather than guessing a name.
    ///
    /// Only ever answers for [`is_subagent_spawn`](Self::is_subagent_spawn)
    /// calls: an ordinary tool with a stray `model` argument is not a spawn.
    pub fn subagent_model(&self) -> Option<&str> {
        if !self.is_subagent_spawn() {
            return None;
        }
        let input = match self {
            ToolCall::Unknown { input, .. } | ToolCall::Mcp { input, .. } => input.as_ref()?,
            _ => return None,
        };
        SUBAGENT_MODEL_KEYS
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|model| !model.is_empty())
    }
}

/// Spawn-input keys that carry a child model, in precedence order. Drivers
/// disagree on the spelling, so the lookup is by key set, not by provider —
/// a new adapter naming it any of these needs no code change here.
pub const SUBAGENT_MODEL_KEYS: [&str; 4] = ["model", "modelId", "model_id", "subagent_model"];

/// The spawn-input keys [`sanitize_tool_call`](crate::) must preserve so the
/// chip can name the child's model. Deliberately tiny: everything else on a
/// spawn's input (the whole prompt, most of all) stays host-local.
pub const SUBAGENT_INPUT_KEEP: [&str; 5] = [
    "model",
    "modelId",
    "model_id",
    "subagent_model",
    "subagent_type",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

/// A slash command advertised by the agent (ACP `availableCommands`): typed as
/// `/name` at the start of the composer, sent to the agent as prompt text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Placeholder hint for the command's argument, when it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

/// A file modification carried inline on a tool result (ACP
/// `ToolCallContent::Diff`). `old_text: None` means a new file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneStatus {
    Completed,
    Interrupted,
    Errored,
}

/// The normalized streaming event emitted by Holt's agent runtime.
///
/// Mirrors holt's `AgentEvent` tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    #[serde(rename_all = "camelCase")]
    SessionStarted {
        provider: String,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
        cwd: String,
        assistant_message_id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Backend-internal steering boundary marker.
    #[serde(rename_all = "camelCase")]
    AssistantMessageCompleted {
        assistant_message_id: String,
    },
    ToolCall {
        id: String,
        call: ToolCall,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        is_error: bool,
        /// Tool output text, capped by the emitting provider (ACP tool-call
        /// content; claude/codex adapters never populate it). The doc-side
        /// fold applies its own byte cap before anything persists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Inline file diff for edit-shaped tools (ACP `Diff` content).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
    },
    /// The agent advertised (or changed) its slash-command set — ACP
    /// `available_commands_update`. The engine caches the latest list per
    /// provider for the composer's `/` popup; never persisted to docs.
    #[serde(rename_all = "camelCase")]
    AvailableCommands {
        commands: Vec<SlashCommand>,
    },
    Error {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    InputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    #[serde(rename_all = "camelCase")]
    InputResolved {
        request_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steered {
        assistant_message_id: Option<String>,
        next_assistant_message_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
    },
    /// A USER-role message injected into a running session — today only seen
    /// wrapped in [`AgentEvent::Subagent`]: the PARENT agent steering its
    /// subagent mid-run (claude: a tagged user frame's text blocks). The
    /// engine writes it to the subagent doc as its own user entry, closing
    /// the streaming assistant segment above it — the subagent transcript
    /// then reads like any steered chat. Never emitted untagged (the parent
    /// chat's user messages come from doc commands, not the wire).
    #[serde(rename_all = "camelCase")]
    UserMessage {
        text: String,
    },
    /// An event belonging to a SUBAGENT's nested transcript, attributed to
    /// the spawning tool call (`parent_tool_use_id` = the parent-feed
    /// `ToolCall::id` that launched it). Never folded into the parent chat
    /// doc — the engine routes these to the subagent's own doc; the parent
    /// keeps only the spawn chip. Additive: old consumers that don't match
    /// this variant drop the nested traffic, which is the pre-subagent-viz
    /// behavior.
    #[serde(rename_all = "camelCase")]
    Subagent {
        parent_tool_use_id: String,
        event: Box<AgentEvent>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_round_trips() {
        let ev = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// `Model.context_window` is the occupancy denominator: builtin rows carry
    /// the catalog truth, custom rows say null, and a row from an older host
    /// (no field at all) reads as unknown instead of failing the decode.
    #[test]
    fn model_context_window_survives_version_skew() {
        let row = |extra: &str| {
            format!(r#"{{"id":"openai/gpt-5.4","provider":"openai","label":"GPT-5.4"{extra}}}"#)
        };
        // Older engine: the key never existed.
        let old: Model = serde_json::from_str(&row("")).unwrap();
        assert_eq!(old.context_window, None);
        // Builtin row: the catalog truth round-trips camelCased.
        let builtin: Model = serde_json::from_str(&row(r#","contextWindow":400000"#)).unwrap();
        assert_eq!(builtin.context_window, Some(400_000));
        assert_eq!(
            serde_json::to_value(&builtin).unwrap()["contextWindow"],
            400_000
        );
        // Unknown window serializes as an explicit null, not a dropped key:
        // absence is reserved for hosts that predate the field.
        assert!(serde_json::to_value(&old).unwrap()["contextWindow"].is_null());
    }

    /// A usage frame is a typed contract, not an ad-hoc blob: its keys are
    /// camelCase on the wire, and a frame from a host that predates a field
    /// (or a future one's unknown source kind) still decodes.
    #[test]
    fn chat_usage_frames_round_trip_and_tolerate_a_narrower_peer() {
        let frame = ChatUsage {
            gross: 780,
            by_kind: BTreeMap::from([(
                "auto-review".to_string(),
                ChatUsageTokens {
                    input: 700,
                    output: 70,
                    cache_read: 7,
                    cache_write: 3,
                },
            )]),
            record_count: 2,
            occupancy: ChatOccupancy {
                tokens: 710,
                context_window: Some(272_000),
                estimated: false,
            },
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["gross"], 780);
        assert_eq!(value["recordCount"], 2);
        assert_eq!(value["byKind"]["auto-review"]["cacheRead"], 7);
        assert_eq!(value["occupancy"]["contextWindow"], 272_000);
        assert_eq!(value["occupancy"]["estimated"], false);
        assert_eq!(serde_json::from_value::<ChatUsage>(value).unwrap(), frame);

        // An older engine's frame (nothing but the total) reads as zeros
        // rather than failing the client's decode.
        let older: ChatUsage = serde_json::from_str(r#"{"gross":42}"#).unwrap();
        assert_eq!(older.gross, 42);
        assert!(older.by_kind.is_empty());
        assert_eq!(older.record_count, 0);
        assert_eq!(older.occupancy, ChatOccupancy::default());

        // A source kind this host has never heard of is data, not an error.
        let newer = serde_json::json!({
            "gross": 1,
            "byKind": { "goal-verifier": { "input": 5 } },
            "recordCount": 1,
            "occupancy": { "tokens": 5, "contextWindow": null, "estimated": true },
        });
        let decoded: ChatUsage = serde_json::from_value(newer).unwrap();
        assert_eq!(decoded.by_kind["goal-verifier"].input, 5);
        assert!(decoded.occupancy.context_window.is_none());
        assert!(decoded.occupancy.estimated);
    }

    /// Drivers spell the key differently; the chip must not care which one
    /// spawned the child. Non-spawns never answer, whatever they carry.
    #[test]
    fn subagent_model_reads_every_spelling_and_only_off_a_spawn() {
        let spawn = |input: serde_json::Value| ToolCall::Unknown {
            name: "Agent: scan".into(),
            input: Some(input),
        };
        for key in SUBAGENT_MODEL_KEYS {
            let call = spawn(serde_json::json!({ key: "haiku" }));
            assert_eq!(call.subagent_model(), Some("haiku"), "key {key}");
        }
        // An MCP-shaped spawn (cursor routes its `task` through MCP) too.
        assert_eq!(
            ToolCall::Mcp {
                server: "s".into(),
                tool: "Agent: scan".into(),
                input: Some(serde_json::json!({ "model": "sonnet" })),
            }
            .subagent_model(),
            Some("sonnet")
        );
        // Not a spawn: the name gate wins over the key.
        assert_eq!(
            ToolCall::Unknown {
                name: "Bash".into(),
                input: Some(serde_json::json!({ "model": "haiku" })),
            }
            .subagent_model(),
            None
        );
        // A spawn that named nothing usable inherits — nothing to render.
        assert_eq!(
            spawn(serde_json::json!({ "model": " " })).subagent_model(),
            None
        );
        assert_eq!(
            spawn(serde_json::json!({ "prompt": "x" })).subagent_model(),
            None
        );
        assert_eq!(
            ToolCall::Unknown {
                name: "Agent".into(),
                input: None
            }
            .subagent_model(),
            None
        );
        // Non-string values are not names.
        assert_eq!(
            spawn(serde_json::json!({ "model": 5 })).subagent_model(),
            None
        );
    }

    #[test]
    fn permission_mode_serializes_kebab_case_and_remaps_legacy_values() {
        let round = |mode: PermissionMode| {
            serde_json::to_value(mode)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(round(PermissionMode::ConfirmChanges), "confirm-changes");
        assert_eq!(round(PermissionMode::AutoReview), "auto-review");
        assert_eq!(round(PermissionMode::FullAccess), "full-access");
        // Round-trip through JSON.
        for mode in [
            PermissionMode::ConfirmChanges,
            PermissionMode::AutoReview,
            PermissionMode::FullAccess,
        ] {
            let json = serde_json::to_value(mode).unwrap();
            assert_eq!(
                serde_json::from_value::<PermissionMode>(json).unwrap(),
                mode
            );
        }
        // Stored sandbox-era values remap on read (ADR-0014): everything
        // except danger-full-access lands on confirm-changes.
        let decode = |value: &str| {
            serde_json::from_value::<PermissionMode>(serde_json::json!(value)).unwrap()
        };
        assert_eq!(decode("workspace-write"), PermissionMode::ConfirmChanges);
        assert_eq!(decode("read-only"), PermissionMode::ConfirmChanges);
        assert_eq!(decode("danger-full-access"), PermissionMode::FullAccess);
        // Unknown values fall back to the first-launch default instead of
        // failing startup.
        assert_eq!(decode("bogus-tier"), PermissionMode::ConfirmChanges);
        assert_eq!(decode(""), PermissionMode::ConfirmChanges);
        // A non-string value is a decode error, not a fallback: old writers
        // always serialized a string, and the lenient readers around stored
        // configs (skip-not-fail) contain the rare stranger.
        assert!(serde_json::from_value::<PermissionMode>(serde_json::Value::Null).is_err());
    }

    #[test]
    fn run_request_reads_legacy_sandbox_field_and_new_key() {
        let base = r#"{"prompt":"p","provider":"openai","model":"openai/gpt-5.4","reasoning":null,"cwd":".""#;
        // The old key + old tier decodes through alias + remap…
        let req: RunRequest =
            serde_json::from_str(&format!(r#"{base},"sandbox":"danger-full-access"}}"#)).unwrap();
        assert_eq!(req.permission_mode, PermissionMode::FullAccess);
        // …the new key round-trips camelCased…
        let req: RunRequest =
            serde_json::from_str(&format!(r#"{base},"permissionMode":"auto-review"}}"#)).unwrap();
        assert_eq!(req.permission_mode, PermissionMode::AutoReview);
        assert_eq!(
            serde_json::to_value(&req).unwrap()["permissionMode"],
            "auto-review"
        );
        // …and a payload without the field defaults to confirm-changes.
        let req: RunRequest = serde_json::from_str(&format!("{base}}}")).unwrap();
        assert_eq!(req.permission_mode, PermissionMode::ConfirmChanges);
    }

    #[test]
    fn run_request_attachments_default_and_round_trip() {
        let old = r#"{"prompt":"p","provider":"openai","model":"openai/gpt-5.4","reasoning":null,"cwd":".","sandbox":"workspace-write"}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.attachments.is_empty());
        // …and an empty list serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("attachments").is_none());
        // Populated lists round-trip.
        let req = RunRequest {
            attachments: vec!["/tmp/a.png".into()],
            ..req
        };
        let round: RunRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(round.attachments, vec!["/tmp/a.png".to_string()]);
    }

    #[test]
    fn run_request_worktree_default_and_round_trip() {
        let old = r#"{"prompt":"p","provider":"openai","model":"openai/gpt-5.4","reasoning":null,"cwd":".","sandbox":"workspace-write"}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.worktree.is_none());
        // …and `None` serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("worktree").is_none());
        // A populated spec round-trips camelCased.
        let req = RunRequest {
            worktree: Some(WorktreeSpec {
                repo_path: "/repos/holt".into(),
                base: "main".into(),
            }),
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["worktree"]["repoPath"], "/repos/holt");
        let round: RunRequest = serde_json::from_value(json).unwrap();
        assert_eq!(round.worktree, req.worktree);
    }
}
