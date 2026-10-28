//! The transcript row model: one row per markdown top-level block plus
//! tool groups, chips, and user bubbles, with stable ids, content-version
//! fingerprints, and the entry→rows construction rules. Pure over `holt-doc`
//! parts — no GPUI, no entity
//! state.

use std::ops::Range;
use std::sync::Arc;

use gpui::SharedString;
use holt_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus, ToolGate,
    ToolGateState,
};
use holt_proto::ToolCall;
use holt_proto::view::{single_line, tool_chip_content};
use holt_rpc::retries::TurnRetryNotice;

use super::markdown::thought_lines;
use super::tool::{OUTPUT_DETAIL_MAX_LINES, ToolDetail, call_block, tool_detail};
use crate::markdown::parser::BlockTree;
use crate::markdown::render;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub call: ToolCall,
    /// The tool part's id — an agent chip's identity key from its very first
    /// paint, before the engine stamps `subagent_ref` (the ref arrives on
    /// this same part, so the key never changes under the chip).
    pub call_id: Option<SharedString>,
    pub is_error: bool,
    pub resolved: bool,
    /// Expandable detail: a code-block of output lines, or a real diff
    /// section rendered by the changes pane's component (ACP providers).
    /// Precomputed here because rows are cached by fingerprint — diffing and
    /// tokenizing per paint would run on every scroll frame.
    pub detail: Option<Arc<ToolDetail>>,
    /// Expandable full-invocation block: the complete tool call (whole
    /// command / pattern / URL / input JSON) that the chip header collapses
    /// to one truncated line. Rendered above `detail` in the open card.
    /// Precomputed for the same reason as `detail`.
    pub invocation: Option<Arc<ToolDetail>>,
    /// Sidecar key of the full output (chat2-sync A3) — the doc carries only
    /// a one-line summary; expanding offers a lazy "Show full output" fetch.
    pub output_ref: Option<SharedString>,
    /// Full-output size, for the affordance label ("Show full output (12 KB)").
    pub output_bytes: Option<u64>,
    /// Sidecar key of the full diff (doc carries only per-file stats).
    pub diff_ref: Option<SharedString>,
    /// The spawned SUBAGENT's doc id — the chip IS the index (there is no
    /// listing endpoint); with it the chip offers "Open subagent".
    pub subagent_ref: Option<SharedString>,
    /// Subagent lifecycle, distinct from `resolved` (eager-done: the spawn
    /// tool's own result lands while the subagent still runs).
    pub subagent_status: Option<SubagentStatus>,
    /// One-line live tail — LEGACY docs only (new runs stopped folding it;
    /// per-delta header rewrites read as noise). Never rendered; still
    /// fingerprinted so an old doc's chips re-splice correctly.
    pub subagent_tail: Option<SharedString>,
    pub subagent_usage: Option<u64>,
    /// A REASONING part riding the tool group as a chip (user request: the
    /// thought process belongs inside the combined "Ran N commands"
    /// accordion, opening/closing with the same tween). Synthesized in
    /// [`rows_for_entry`] — never comes from a doc tool part. The thought
    /// text is the `detail`; the chip defaults collapsed, streaming or
    /// settled. `resolved == false` only marks the part as still streaming.
    pub is_thought: bool,
    /// The permission gate's record (ADR-permission-modes-gate-mutations-no-os-sandbox), when this call was gated.
    /// A `Pending` gate renders only in the composer's approval bar — the
    /// transcript skips the part — so a gate seen here is always settled,
    /// and the chip carries its verdict.
    pub gate: Option<ToolGate>,
}

/// Subagent spawn chips — [`ToolCall::is_subagent_spawn`], the shared genus
/// every driver decodes its spawn tool into. These stay out of the
/// collapsible "Called N tools" wrap so a running subagent is visible
/// without opening the fold.
pub(super) fn is_agent_call(call: &ToolCall) -> bool {
    call.is_subagent_spawn()
}

/// The chip's GENUS is the call itself, never the ref: docs written before
/// the claude-driver fix carry stray `subagent_ref`s on ordinary Run chips
/// (a background shell's `task_notification` was mis-tagged as subagent
/// traffic), and honoring the ref alone turned those Runs into spawn chips
/// that opened empty, never-created subagent docs.
pub(super) fn is_agent_tool(item: &ToolItem) -> bool {
    is_agent_call(&item.call)
}

/// A chip renders as the spawn LINK (whole-card click → subagent tab) only
/// when an agent call has actually been bound to its doc.
pub(super) fn is_spawn_link(item: &ToolItem) -> bool {
    is_agent_call(&item.call) && item.subagent_ref.is_some()
}

/// A `Workflow` call's part: bare while its arguments stream, `Workflow:
/// <name>` once the engine launched the run.
pub(super) fn is_workflow_call(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Unknown { name, .. } if name == "Workflow" || name.starts_with("Workflow: "))
}

/// The workflow phase a child's chip was started under (the kept spawn
/// input), which splits a workflow's children into one row per phase.
pub(super) fn tool_phase(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::Unknown {
            input: Some(input), ..
        } if call.is_subagent_spawn() => input.get("phase")?.as_str(),
        _ => None,
    }
}

fn is_fanout_container(parts: &[MessagePart], id: &str) -> bool {
    let prefix = format!("{id}#item-");
    parts
        .iter()
        .any(|part| matches!(part, MessagePart::Tool { id, .. } if id.starts_with(&prefix)))
}

/// Ordinary tool groups fold behind a summary header; agent-only groups use
/// the renderer's one-line expandable fan-out row.
pub(super) fn tool_group_collapses(tools: &[ToolItem]) -> bool {
    tools.iter().any(|t| !is_agent_tool(t))
}

/// A reasoning part as a tool-group chip: "Thought process" header over the
/// thought's markdown flattened into styled detail lines (analytic height —
/// the group's fold tween needs it; see [`thought_lines`]). Capped like tool
/// outputs, with the counted tail. `live` = the part is still streaming.
fn thought_item(tree: &BlockTree, live: bool) -> ToolItem {
    let mut lines = thought_lines(tree);
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    if truncated_by > 0 {
        // Keep the TAIL while streaming (the fresh thinking is the signal);
        // settled thoughts keep the head like tool outputs do.
        if live {
            lines.drain(..truncated_by);
            // The cut can land on a block separator — drop the orphan blank.
            while lines
                .first()
                .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
            {
                lines.remove(0);
            }
        } else {
            lines.truncate(OUTPUT_DETAIL_MAX_LINES);
        }
    }
    ToolItem {
        call: ToolCall::Unknown {
            name: "Thought process".into(),
            input: None,
        },
        call_id: None,
        is_error: false,
        resolved: !live,
        detail: (!lines.is_empty()).then(|| {
            Arc::new(ToolDetail::Thought {
                lines,
                truncated_by,
            })
        }),
        invocation: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
        subagent_usage: None,
        is_thought: true,
        gate: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSkill {
    pub name: SharedString,
    pub file: SharedString,
}

/// Format a kebab-case or snake_case skill name into Title Case for user bubble display
/// (e.g. "ask-matt" -> "Ask Matt", "code-review" -> "Code Review").
pub fn format_skill_title(name: &str) -> String {
    name.split(['-', '_'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The invocation chip's source-file pointer with the home directory
/// collapsed to `~`: the absolute path is noise in a 12px detail slot, and
/// `~/.agents/skills/…` reads as the same file at a glance.
pub(super) fn skill_file_display(file: &str) -> String {
    match home_dir() {
        Some(home) => skill_file_display_on(cfg!(windows), &home, file),
        None => file.trim_start_matches("file://").to_string(),
    }
}

/// [`skill_file_display`] for an explicit platform and home — see
/// [`display_combo_on`](crate::settings::display_combo_on) for the same
/// pattern: the platform branch is the thing worth testing, so it is a
/// parameter rather than a `cfg!` only one CI leg can reach.
///
/// Home arrives from the environment in the host's spelling (`C:\Users\…` on
/// Windows) while skill paths ride the RPC's addressed one (`C:/Users/…`), so
/// Windows folds both. Only there: a backslash in a unix filename is a
/// character, not a separator, and the display keeps the file's real spelling.
fn skill_file_display_on(windows: bool, home: &str, file: &str) -> String {
    let path = file.trim_start_matches("file://");
    if windows {
        collapse_home(&path.replace('\\', "/"), &home.replace('\\', "/"))
    } else {
        collapse_home(path, home)
    }
}

/// `~` in place of a leading `home`, matched as whole components:
/// `/home/onion2/x` shares the string prefix but is not under `/home/onion`,
/// so it must not collapse to `~2/x`. A trailing separator on `home` is fine.
fn collapse_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// The user's home directory — the same resolution chain the engine's
/// `local_fs::home_dir` uses (the UI cannot import it across the RPC
/// boundary): `HOME`, else `USERPROFILE`, else `HOMEDRIVE`+`HOMEPATH`. A
/// plain Windows process has no `HOME`, and without the fallback the `~`
/// collapse silently stops working there.
fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut home = drive;
            home.push(path);
            Some(home)
        })
        .map(|home| home.to_string_lossy().to_string())
        .filter(|home| !home.is_empty())
}

#[derive(Clone)]
pub enum RowKind {
    User {
        /// Visible prompt (attachment-ref trailer already stripped). When the
        /// prompt carries file mentions this is the *projected* display text —
        /// chip labels in place of the raw Markdown links.
        text: SharedString,
        /// The pre-projection body the display text came from: the raw side
        /// of the selection copy map, so Cmd+C yields canonical Markdown.
        raw: SharedString,
        /// File-mention chips over `text`, in display-byte terms. Computed
        /// once per entry change in [`rows_for_entry`] (rows are cached by
        /// fingerprint), never per frame. Empty for ordinary prompts.
        mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
        /// Image refs parsed out of the message text (message-attachments.ts):
        /// thumbnails load from the owning device via ReadAttachmentChunk.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Context the prompt folded in as text, lifted back out by `badges`.
        badges: Arc<Vec<crate::badges::MessageBadge>>,
        /// Skill invocation metadata when the turn was launched via `/skill`.
        skill: Option<Arc<UserSkill>>,
        /// The arming send (`/goal <objective>`): the bubble heads its text
        /// with the white `goal` chip (icon gutter + bare name — the
        /// composer's own treatment) instead of a pill above it.
        goal_chip: bool,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message. Split per block like
    /// completed rows (only the tail blocks' versions change per commit, so
    /// the settled prefix is never respliced or re-rendered); rendered with
    /// the fade veil.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        /// First question's header (chat-view.tsx `InputChip`: the resolved
        /// chip shows it; unresolved shows "Awaiting your answer…" — which
        /// stays TRUE even across a run death: the composer keeps the panel
        /// up until the user answers, and the engine delivers a dead run's
        /// answer as a resumed turn).
        header: SharedString,
        resolved: bool,
    },
    /// A skill invocation / skill-file read, collapsed (ADR-metadata-only-skill-advertising): header
    /// `[icon] Skill <name>` plus the source-file pointer. When `content`
    /// is present (an invocation) the chip expands thinking-style to show
    /// the exact `<skill>` block the model received; read-collapse chips
    /// have none and stay a one-line pointer.
    SkillChip {
        name: SharedString,
        /// Absolute `SKILL.md` path; shown as the header's muted tail.
        file: SharedString,
        /// The model-visible invocation block — the expandable body.
        content: Option<SharedString>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    ErrorChip {
        message: SharedString,
    },
    /// A quiet full-width housekeeping row (ADR-history-is-a-separate-persisted-record): the legacy-chat and
    /// damaged-History notices — later the overflow and failed-compaction
    /// messages ride the same shape. Not an error: nothing went wrong with
    /// the Turn it sits in; it states where the model's memory begins.
    Notice {
        message: SharedString,
    },
    /// The goal loop's terminal row (ADR-goal-mode-verified-loop-at-turn-settle): the goal card's settled
    /// form landed in the transcript — the same target glyph on a
    /// passive band, so it reads as the arc's conclusion, not housekeeping
    /// noise. Model-side it is a `Notice` the engine pushed under a
    /// `goal-*` entry id.
    GoalEnd {
        message: SharedString,
        event: holt_proto::GoalEvent,
    },
    /// A goal-loop continuation (ADR-goal-mode-verified-loop-at-turn-settle): a User entry the loop queued,
    /// not the user. Drawn as a light, foldable loop row — round label and
    /// the verifier's missing piece — instead of a user bubble; the fold
    /// shows the full prompt the model got.
    /// A background workflow run (ADR-workflows-run-scripts-in-the-background-and-report-by-continuation):
    /// the `Workflow` call's own row — its name, lifecycle, and tokens; the
    /// run's agents follow as fan-out rows, one per phase.
    Workflow {
        call_id: SharedString,
        name: SharedString,
        status: Option<SubagentStatus>,
        tokens: Option<u64>,
    },
    GoalRound {
        label: SharedString,
        reason: SharedString,
        prompt: SharedString,
        /// The entry whose footer strip (timestamp + copy) the row took over
        /// ([`hand_footers_to_goal_rounds`]): drawn inline at the row's
        /// right edge and lit by hovering that entry. Kept apart from the
        /// row's own `entry_id`, which anchors the Turn it opens.
        footer_owner: Option<SharedString>,
    },
    /// The Compaction divider (ADR-compaction-is-not-a-turn): where the model's verbatim memory
    /// begins. Collapsed by default; expands to the exact summary the model
    /// carries, rendered as Markdown.
    CompactionDivider {
        summary: Arc<BlockTree>,
    },
    /// The Plan Mode approval card (ADR-plan-mode-is-an-independent-approval-checkpoint): one per plan submission,
    /// carrying the document pointer and its resolution state. Pending
    /// cards show the three verdict affordances (approve / reject with
    /// feedback / stay in planning); settled cards show their verdict
    /// marker. Interactive state (the feedback editor) lives on the
    /// Transcript entity keyed by plan id, never here.
    PlanApproval {
        /// The proposed plan's Markdown — the card renders what was
        /// proposed (a `<proposed_plan>` block from the assistant text).
        content: SharedString,
        state: holt_doc::parts::PlanApprovalState,
    },
    /// A Provider Mode proposal card (ADR-provider-mode-is-a-chat-mode): the stored proposal's
    /// summary and change lines. Write / Discard while pending; the
    /// buttons' busy/error state lives on the Transcript keyed by row id.
    ModelProposal {
        proposal_id: SharedString,
        /// The providers the batch writes to — the card's header, so the
        /// user sees where it lands.
        targets: Vec<holt_doc::ProviderRef>,
        summary: SharedString,
        lines: Vec<SharedString>,
        state: holt_doc::ProposalCardState,
    },
    /// A Provider Mode provider choice (ADR-provider-mode-is-a-chat-mode): an organization's
    /// candidate providers, filled by the engine from the catalog. A click
    /// settles it; `chosen` is the stamped pick.
    ProviderChoice {
        card_id: SharedString,
        options: Vec<holt_doc::ProviderRef>,
        chosen: Option<SharedString>,
        state: holt_doc::ChoiceCardState,
    },
    /// The agent's question card (ADR-ask-user-is-a-question-card): 1–4 questions the model
    /// asked through `ask_user`. A pending card renders only in the
    /// composer's approval bar (the gate's rule); the settled card is
    /// the marker row, one line per answered question.
    QuestionCard {
        questions: Arc<Vec<SharedString>>,
        answers: Arc<Vec<SharedString>>,
        state: holt_doc::ChoiceCardState,
    },
    /// A Provider Mode key request (ADR-provider-mode-is-a-chat-mode): who the key unlocks and the
    /// one destination it goes to. The masked input is a Transcript-owned
    /// entity keyed by row id — the key never enters the row.
    KeyRequest {
        provider_name: SharedString,
        destination: SharedString,
        state: holt_doc::KeyCardState,
    },
    /// The Turn's file-change card (ADR-turn-change-sets-and-review ticket 03): what one main-chat
    /// Turn changed so far — or, once it settled (success, failure, or
    /// interruption), what it froze as. Appended after the Turn's last
    /// conversation row (trailing housekeeping System entries render below
    /// it), never part of the entry rows themselves: it rides app state
    /// keyed by the Turn's user-message id, not the doc. An empty change set builds
    /// no row — an empty card is not a result. Review/Open actions are
    /// ticket 04.
    TurnChangeCard {
        change_set: Arc<holt_proto::TurnChangeSet>,
        restore: Option<crate::state::TurnRestoreMark>,
    },
    /// A live provider-retry chip (WatchTurnRetry): the streaming entry's
    /// provider request hit a transient failure and is backing off before
    /// the next attempt. Appended under the streaming entry's rows, riding
    /// app state (never the doc): the chip vanishes when transcript frames
    /// resume, leaving no trace in the record.
    RetryChip {
        /// 1-based number of the retry about to run.
        attempt: u32,
        /// The configured retry budget.
        max_retries: u32,
        /// Engine-stamped epoch ms at which the retry fires; the chip reads
        /// it against the clock on every render, so its label counts down.
        retry_at_ms: i64,
        /// The provider error that triggered the retry.
        error: SharedString,
    },
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: SharedString,
    pub version: u64,
    /// First row of its message entry (gets the turn gap).
    pub turn_start: bool,
    pub kind: RowKind,
    /// The owning message entry — hover anywhere on the entry's rows reveals
    /// its timestamp strip (holt chat-view.tsx `group`/`group-hover`).
    pub entry_id: SharedString,
    /// Epoch-ms for the 16px hover-timestamp strip UNDER this row: set on the
    /// LAST row of a completed entry (user rows always; assistant rows only
    /// once streaming ends — "the turn isn't at a time yet", chat-view.tsx).
    pub timestamp: Option<i64>,
    /// Text copied by the entry-level hover action. Present only on the last
    /// settled row, beside the timestamp; tools and transport-only metadata
    /// are deliberately excluded.
    pub copy_text: Option<SharedString>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM" — the exact
/// `formatTimestamp` shape (utils.ts: short month, numeric day, hour,
/// 2-digit minutes, no leading zero on the hour). Pure over an explicit
/// timezone so tests don't depend on the host's local time.
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

/// Hash a tool's gate into a row/entry fingerprint: the gate settles in
/// place (Pending → Settled, or a persisted doc restamping a stale pending
/// as Aborted) without touching resolved/is_error, so both cache keys must
/// see it or the approval card never folds back into its verdict chip.
/// Shared by [`tool_fingerprint`] and [`entry_fingerprint`] — drift between
/// the two is a stale-cache bug.
fn hash_gate(acc: &mut Vec<u8>, gate: Option<&ToolGate>) {
    match gate {
        None => acc.push(0),
        Some(gate) => {
            acc.extend_from_slice(gate.id.as_bytes());
            if let Some(origin) = &gate.origin {
                acc.extend_from_slice(origin.doc_id.as_bytes());
                acc.push(0);
                acc.extend_from_slice(origin.label.as_bytes());
            }
            match &gate.state {
                ToolGateState::Pending { .. } => acc.push(1),
                ToolGateState::Settled { verdict } => {
                    acc.push(2);
                    acc.extend_from_slice(format!("{verdict:?}").as_bytes());
                }
            }
        }
    }
}

fn tool_fingerprint(tools: &[ToolItem], auto_open: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for t in tools {
        let (label, detail) = tool_chip_content(&t.call);
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(&(detail.len() as u32).to_le_bytes());
        acc.push(t.is_error as u8 | (t.resolved as u8) << 1);
        // Detail payload arriving (or growing) must re-splice the row even
        // when the resolved bit didn't change.
        match t.detail.as_deref() {
            None => acc.push(0),
            Some(ToolDetail::Output {
                lines,
                truncated_by,
            }) => {
                acc.push(1);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                let bytes: usize = lines.iter().map(|l| l.len()).sum();
                acc.extend_from_slice(&(bytes as u32).to_le_bytes());
            }
            Some(ToolDetail::Thought {
                lines,
                truncated_by,
            }) => {
                // Byte-exact plus style bits: a live mend can restyle runs
                // without changing the flattened length, and the row must
                // still re-splice.
                acc.push(4);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                for line in lines {
                    for run in line {
                        acc.extend_from_slice(run.text.as_bytes());
                        acc.push(
                            run.style.bold as u8
                                | (run.style.italic as u8) << 1
                                | (run.style.code as u8) << 2
                                | (run.style.strikethrough as u8) << 3
                                | (run.style.link.is_some() as u8) << 4,
                        );
                    }
                    acc.push(b'\n');
                }
            }
            Some(ToolDetail::Diff { file, .. }) => {
                acc.push(2);
                acc.extend_from_slice(file.path.as_bytes());
                acc.extend_from_slice(&file.additions.to_le_bytes());
                acc.extend_from_slice(&file.deletions.to_le_bytes());
                acc.extend_from_slice(&(file.hunks.len() as u32).to_le_bytes());
            }
            Some(ToolDetail::Stats { stats }) => {
                acc.push(3);
                for stat in stats.iter() {
                    acc.extend_from_slice(stat.path.as_bytes());
                    acc.extend_from_slice(&stat.additions.to_le_bytes());
                    acc.extend_from_slice(&stat.deletions.to_le_bytes());
                }
            }
        }
        // The invocation block is pure over `call`, which the one-line hash
        // above only covers by length — hash its bytes so an in-place call
        // update (a streaming MCP input, a growing todo list) re-splices.
        if let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = t.invocation.as_deref()
        {
            for line in lines {
                acc.extend_from_slice(line.as_bytes());
            }
            acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
        }
        // Sidecar refs arriving after the resolve tick must re-splice too —
        // they add the fetch affordance without changing the detail payload.
        acc.push(t.output_ref.is_some() as u8 | (t.diff_ref.is_some() as u8) << 1);
        // Subagent lifecycle mutates the chip in place (status flips, the
        // live tail grows) — hash it so the row re-splices on every change.
        acc.push(
            t.subagent_ref.is_some() as u8
                | match t.subagent_status {
                    None => 0,
                    Some(SubagentStatus::Running) => 1 << 1,
                    Some(SubagentStatus::Done) => 2 << 1,
                    Some(SubagentStatus::Failed) => 3 << 1,
                },
        );
        if let Some(tail) = &t.subagent_tail {
            acc.extend_from_slice(tail.as_bytes());
        }
        // The gate's verdict flips the chip's verdict marker in place —
        // hashed via the shared helper (see `hash_gate`).
        hash_gate(&mut acc, t.gate.as_ref());
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Clipboard payload for an assistant/system entry: authored text parts in
/// document order, preserving Markdown while excluding tool traces and other
/// structured parts.
fn assistant_copy_text(entry: &SessionMessageEntry) -> Option<SharedString> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            // Inspect the trimmed view only to reject empty parts. Copy the
            // original bytes so indentation-based code blocks and Markdown
            // hard-break whitespace survive the clipboard round trip.
            MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then(|| text.into())
}

/// Build the block rows of one (already continuation-joined) entry.
///
/// `parse` maps `(part_key, text)` to a block tree — the entity supplies
/// incremental parsers for live parts and a cache for complete ones; tests pass
/// a plain `parse_full`.
pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);
    let entry_id: SharedString = entry.id.clone().into();

    if entry.role == MessageRole::User {
        let skill = entry.parts.iter().find_map(|p| match p {
            // The user bubble keeps the compact tag (name + source
            // pointer); the `<skill>` block rides the agent entry's chip.
            MessagePart::Skill { name, file, .. } => Some(Arc::new(UserSkill {
                name: name.clone().into(),
                file: file.clone().into(),
            })),
            _ => None,
        });
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // A loop-queued continuation carries its round as a part (ADR-goal-mode-verified-loop-at-turn-settle):
        // it is the loop speaking, so it never draws as a user bubble.
        let goal_round = entry.parts.iter().find_map(|p| match p {
            MessagePart::GoalRound { round, reason, .. } => {
                Some((crate::goal::round_label(round), reason.clone()))
            }
            _ => None,
        });
        // A workflow's report (ADR-workflows-run-scripts-in-the-background-and-report-by-continuation) is
        // the engine speaking too: the same light, foldable row, whose fold
        // shows the result the model received.
        let goal_round = goal_round.or_else(|| {
            entry.parts.iter().find_map(|p| match p {
                MessagePart::WorkflowResult { name, failed, .. } => Some((
                    if *failed {
                        crate::i18n::t!(
                            "conversation_transcript.workflow.report_failed",
                            name = name
                        )
                    } else {
                        crate::i18n::t!("conversation_transcript.workflow.report", name = name)
                    }
                    .into_owned(),
                    String::new(),
                )),
                _ => None,
            })
        });
        if let Some((label, reason)) = goal_round {
            rows.push(Row {
                id: entry.id.clone().into(),
                version: (raw.len() as u64) << 1 | pending as u64,
                turn_start: true,
                kind: RowKind::GoalRound {
                    label: label.into(),
                    reason: reason.into(),
                    prompt: raw.into(),
                    footer_owner: None,
                },
                entry_id,
                timestamp: None,
                copy_text: None,
            });
            return rows;
        }
        // Attachment refs ride the plain text (the `withAttachments`
        // transport); split them back out for the thumbnail strip.
        let mut parsed = crate::attachments::parse_user_message_images(&raw);
        // File mentions render as chips here too, not just in the composer.
        // The projection is pure over the text, so the raw-length row version
        // below stays a valid cache/diff key.
        // The composer folds its trailers in a fixed order — the goal
        // arming marker onto the objective, then the comment block, then the
        // path list — and each one is matched only as a TRAILING block, so
        // they peel off in reverse.
        //
        // The appended path list leaves the bubble entirely: it rides the
        // prompt for the model, but on screen the references render as an
        // attachment chip row above the bubble (one badge per target, full
        // path on hover), next to the comment badge.
        let (body, references) = crate::path_refs::split_sent_references(&parsed.text);
        // Lifted before the mention projection, so a comment body's own
        // Markdown never lands in the bubble.
        let (body, badges) = crate::badges::split(&body);
        // No pill replaces the goal marker: the renderer chips the `goal`
        // directive inline at the bubble's head (the composer's treatment).
        let (body, goal_chip) = match crate::goal::strip_block(&body) {
            Some(objective) => (objective, true),
            None => (body, false),
        };
        parsed.attachments.extend(
            references
                .iter()
                .filter(|r| !r.is_dir && crate::images::is_image_path(&r.path))
                .map(|r| crate::attachments::UserImageAttachment {
                    id: r.path.clone(),
                    path: r.path.clone(),
                    name: r.label.clone(),
                }),
        );
        let mut badges = badges;
        badges.extend(
            references
                .iter()
                .filter(|r| r.is_dir || !crate::images::is_image_path(&r.path))
                .map(|reference| crate::badges::MessageBadge {
                    icon: if reference.is_dir {
                        crate::icons::FOLDER
                    } else {
                        crate::icons::DOCUMENT
                    },
                    label: reference.label.clone().into(),
                    details: vec![crate::badges::BadgeDetail {
                        location: reference.label.clone().into(),
                        tag: None,
                        body: reference.path.clone().into(),
                    }],
                }),
        );
        // Legacy holt-file: mentions project first; new inline path
        // references (quoted absolute paths inside the composed text)
        // collapse to the same chips. Both are pure over the text, so the
        // raw-length row version below stays a valid cache/diff key. `raw`
        // (the pre-projection body) rides the row so copy yields the
        // canonical Markdown, never the displayed chip labels.
        let (text, mentions) = match crate::composer::sent_mention_display(&body) {
            Some((display, spans)) => (display, spans),
            None => match crate::path_refs::sent_reference_display(&body) {
                Some((display, spans)) => (display, spans),
                None => (body.clone(), Vec::new()),
            },
        };
        // The hover strip copies what was SENT: raw body for ordinary rows,
        // the legacy directive shape for legacy skill-chip rows.
        let copy_text = match (&skill, !body.trim().is_empty()) {
            (Some(skill), true) => {
                Some(SharedString::from(format!("/skill {} {body}", skill.name)))
            }
            (Some(skill), false) => Some(SharedString::from(format!("/skill {}", skill.name))),
            (None, true) => Some(SharedString::from(body.clone())),
            (None, false) => None,
        };
        let skill_fp = skill.as_ref().map_or(0, |s| {
            fnv1a(format!("{}\u{0}{}", s.name, s.file).as_bytes())
        });
        let version = ((raw.len() as u64) ^ skill_fp) << 1 | pending as u64;

        if !raw.trim().is_empty() || skill.is_some() {
            rows.push(Row {
                id: entry.id.clone().into(),
                version,
                turn_start: true,
                kind: RowKind::User {
                    text: text.into(),
                    // The pre-projection body: the copy map's raw side.
                    raw: SharedString::from(body),
                    mentions: Arc::new(mentions),
                    attachments: Arc::new(parsed.attachments),
                    badges: Arc::new(badges),
                    skill,
                    goal_chip,
                    pending,
                },
                entry_id,
                // User rows always carry the strip (chat-view.tsx: whenever
                // `createdAt` exists — the optimistic echo included).
                timestamp: Some(entry.created_at),
                copy_text,
            });
        }
        return rows;
    }

    // Assistant/system: split parts into block rows, folding consecutive
    // ordinary tools. Agent/spawn chips flush into their own group so they
    // never share a collapse with Reads/Runs.
    let last_part_ix = entry.parts.len().saturating_sub(1);
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let auto_open = streaming && last_ix == last_part_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix).into(),
                version: tool_fingerprint(&tools, auto_open),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    auto_open,
                },
                entry_id: entry.id.clone().into(),
                timestamp: None,
                copy_text: None,
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Skill {
                file,
                content: None,
                ..
            } => {
                // Skill-file reads belong with the assistant's other tools.
                if pending_group.first().is_some_and(is_agent_tool) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                let call = ToolCall::ReadFile { path: file.clone() };
                pending_group.push(ToolItem {
                    invocation: call_block(&call).map(Arc::new),
                    call,
                    call_id: None,
                    is_error: false,
                    resolved: true,
                    detail: None,
                    output_ref: None,
                    output_bytes: None,
                    diff_ref: None,
                    subagent_ref: None,
                    subagent_status: None,
                    subagent_tail: None,
                    subagent_usage: None,
                    is_thought: false,
                    gate: None,
                });
                group_last_part_ix = part_ix;
            }
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
                subagent_usage,
                gate,
            } => {
                // A workflow call is its run's own row; its agents follow.
                if is_workflow_call(call) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                    let (_, name) = tool_chip_content(call);
                    rows.push(Row {
                        id: format!("{}#{}", entry.id, id).into(),
                        version: fnv1a(
                            format!("{name}\0{subagent_status:?}\0{subagent_usage:?}").as_bytes(),
                        ),
                        turn_start: false,
                        kind: RowKind::Workflow {
                            call_id: id.clone().into(),
                            name: name.into(),
                            status: *subagent_status,
                            tokens: *subagent_usage,
                        },
                        entry_id: entry.id.clone().into(),
                        timestamp: None,
                        copy_text: None,
                    });
                    continue;
                }
                // A fan-out `Agent` call is drawn by its per-item spawn parts
                // (`{id}#item-{n}`, engine `subagents::execute_fanout`); the
                // container chip itself would only repeat them.
                if call.is_subagent_spawn() && is_fanout_container(&entry.parts, id) {
                    continue;
                }
                let item = ToolItem {
                    call: call.clone(),
                    call_id: Some(SharedString::from(id.clone())),
                    is_error: *is_error,
                    resolved: *resolved,
                    detail: tool_detail(output.as_deref(), diff.as_ref(), diff_stats.as_deref())
                        .map(Arc::new),
                    invocation: call_block(call).map(Arc::new),
                    output_ref: output_ref.clone().map(SharedString::from),
                    output_bytes: *output_bytes,
                    diff_ref: diff_ref.clone().map(SharedString::from),
                    subagent_ref: subagent_ref.clone().map(SharedString::from),
                    subagent_status: *subagent_status,
                    subagent_tail: subagent_tail.clone().map(SharedString::from),
                    subagent_usage: *subagent_usage,
                    is_thought: false,
                    gate: gate.clone(),
                };
                // A PENDING gate builds no transcript row (ADR-permission-modes-gate-mutations-no-os-sandbox): the
                // composer's approval bar carries the gated target and the
                // verdict affordances, so the part never joins a foldable
                // group. The settle replays the same part through the
                // ordinary path below, which lands it in a group as the
                // verdict chip.
                if matches!(
                    item.gate.as_ref().map(|gate| &gate.state),
                    Some(ToolGateState::Pending { note: None })
                ) {
                    // The skip steals the entry's TAIL part from the group
                    // above: without the gate that group IS the live tail of
                    // the still-streaming turn (a running bash call renders
                    // expanded), and the pause waiting on a verdict is not
                    // the group finishing. Flush with the tail's index so the
                    // group keeps the auto_open it would have had gate-free;
                    // a stale pending gate mid-entry flushes normally.
                    let flush_tail_ix = if part_ix == last_part_ix {
                        last_part_ix
                    } else {
                        group_last_part_ix
                    };
                    flush_group(&mut rows, &mut pending_group, &mut group_ix, flush_tail_ix);
                    continue;
                }
                // Agent chips don't share a fold with ordinary tools: flush
                // whenever the genus flips so each group is uniform.
                // A workflow's agents split further, one row per phase.
                if pending_group.first().is_some_and(|head| {
                    is_agent_tool(head) != is_agent_tool(&item)
                        || tool_phase(&head.call) != tool_phase(&item.call)
                }) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            // Thinking rides the SAME accordion as the tools around it
            // (user request) — a thought chip in the group, not its own row.
            MessagePart::Reasoning { id: part_id, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                // Live only while it is the tail of a streaming reply — once
                // text or a tool follows, the thought is finished even though
                // the entry still streams.
                let live = streaming && part_ix == last_part_ix;
                // The same parse wiring as text parts: incremental while
                // streaming, hanging inline markers mended for display, the
                // settled cache once complete.
                let tree = parse(&format!("{}#{}", entry.id, part_id), text);
                let item = thought_item(&tree, live);
                // Thoughts join ordinary tool groups; agent (spawn-link)
                // groups stay pure, exactly like the tool genus rule.
                if pending_group.first().is_some_and(is_agent_tool) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            // The task list rides the top-right capsule (issue #47's
            // replaced surface) — the transcript part is its record and
            // transport, never a row. Fully invisible here: no flush, so
            // the part never splits a tool group either.
            MessagePart::TodoCard { .. } => {}
            other => {
                // A pending question card keeps the group above it as the
                // live tail (the gate's rule): the turn is stopped on the
                // answer, not finished. A stale mid-entry card flushes
                // normally.
                let flush_tail_ix = if part_ix == last_part_ix
                    && matches!(
                        other,
                        MessagePart::QuestionCard {
                            state: holt_doc::ChoiceCardState::Pending,
                            ..
                        }
                    ) {
                    last_part_ix
                } else {
                    group_last_part_ix
                };
                flush_group(&mut rows, &mut pending_group, &mut group_ix, flush_tail_ix);
                match other {
                    MessagePart::Text { id: part_id, text } => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, text);
                        // Live and completed parts split identically — one row
                        // per top-level block, same ids, so the live→complete
                        // handoff never changes row identity. The version is a
                        // content hash of the block's bytes (LSB = streaming),
                        // so a commit only splices rows whose bytes actually
                        // changed — the settled prefix of a live reply is
                        // untouched (and its render caches stay valid).
                        for block_ix in 0..tree.blocks.len() {
                            let range = &tree.blocks[block_ix].range;
                            let end = range.end.min(text.len());
                            let bytes = text
                                .as_bytes()
                                .get(range.start.min(end)..end)
                                .unwrap_or_default();
                            let version = (fnv1a(bytes) << 1) | streaming as u64;
                            rows.push(Row {
                                id: format!("{key}.{block_ix}").into(),
                                version,
                                turn_start: false,
                                entry_id: entry_id.clone(),
                                timestamp: None,
                                copy_text: None,
                                kind: if streaming {
                                    RowKind::LiveMarkdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                } else {
                                    RowKind::Markdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                },
                            });
                        }
                    }
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        // Model-generated header onto the one-line chip.
                        let header: SharedString = single_line(
                            &questions
                                .first()
                                .map(|q| q.header.clone())
                                .unwrap_or_else(|| {
                                    crate::i18n::t!("conversation_transcript.question.label")
                                        .into_owned()
                                }),
                        )
                        .into();
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                header,
                                resolved: *resolved,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Error {
                        id: part_id,
                        message,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: message.len() as u64,
                            turn_start: false,
                            kind: RowKind::ErrorChip {
                                // Provider-generated; the chip is one line.
                                message: single_line(message).into(),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Skill {
                        id: part_id,
                        name,
                        file,
                        content,
                    } => {
                        // The content rides the row version: an invocation
                        // seed (or a doc carrying one) re-keys the row.
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(
                                format!(
                                    "{name}\u{0}{file}\u{0}{}",
                                    content.as_deref().map_or(0, str::len)
                                )
                                .as_bytes(),
                            ),
                            turn_start: false,
                            kind: RowKind::SkillChip {
                                name: name.clone().into(),
                                file: file.clone().into(),
                                content: content.clone().map(SharedString::from),
                                pending: false,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Notice {
                        id: part_id,
                        message,
                        goal,
                    } => {
                        // The goal loop's lifecycle rows (ADR-goal-mode-verified-loop-at-turn-settle) get the
                        // band treatment, keyed on the part's transition
                        // tag; every other Notice stays a plain housekeeping
                        // row.
                        let kind = if let Some(event) = goal {
                            RowKind::GoalEnd {
                                message: message.clone().into(),
                                event: *event,
                            }
                        } else {
                            RowKind::Notice {
                                // Engine-authored prose; the row wraps.
                                message: message.clone().into(),
                            }
                        };
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: message.len() as u64,
                            turn_start: false,
                            kind,
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    // Tools and thoughts are grouped by the outer arms;
                    // nothing reaches here.
                    // A round tag only rides user entries (the loop row
                    // above); on any other entry it draws nothing.
                    // The task list is the capsule's record (the early arm
                    // above) — never a row.
                    MessagePart::Tool { .. }
                    | MessagePart::Reasoning { .. }
                    | MessagePart::GoalRound { .. }
                    | MessagePart::WorkflowResult { .. }
                    | MessagePart::TodoCard { .. } => {}
                    MessagePart::ModelProposal {
                        id: part_id,
                        proposal_id,
                        targets,
                        summary,
                        lines,
                        state,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(format!("{state:?}\0{summary}").as_bytes()),
                            turn_start: false,
                            kind: RowKind::ModelProposal {
                                proposal_id: proposal_id.clone().into(),
                                targets: targets.clone(),
                                summary: summary.clone().into(),
                                lines: lines.iter().cloned().map(SharedString::from).collect(),
                                state: *state,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::ProviderChoice {
                        id: part_id,
                        options,
                        chosen,
                        state,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(
                                format!("{state:?}\0{}", chosen.as_deref().unwrap_or_default())
                                    .as_bytes(),
                            ),
                            turn_start: false,
                            kind: RowKind::ProviderChoice {
                                card_id: part_id.clone().into(),
                                options: options.clone(),
                                chosen: chosen.clone().map(SharedString::from),
                                state: *state,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::QuestionCard {
                        id: part_id,
                        questions,
                        answers,
                        state,
                    } => {
                        // A PENDING question card builds no transcript row
                        // (the gate's rule): the composer's approval bar
                        // carries the questions and their affordances. The
                        // outer arm already flushed the group above with
                        // the tail rule.
                        if *state == holt_doc::ChoiceCardState::Pending {
                            continue;
                        }
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(
                                format!(
                                    "{state:?}\0{}",
                                    answers
                                        .iter()
                                        .map(String::as_str)
                                        .collect::<Vec<_>>()
                                        .join("\0")
                                )
                                .as_bytes(),
                            ),
                            turn_start: false,
                            kind: RowKind::QuestionCard {
                                questions: Arc::new(
                                    questions
                                        .iter()
                                        .map(|q| SharedString::from(q.question.clone()))
                                        .collect(),
                                ),
                                answers: Arc::new(
                                    answers.iter().cloned().map(SharedString::from).collect(),
                                ),
                                state: *state,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::KeyRequest {
                        id: part_id,
                        provider_id,
                        provider_name,
                        destination,
                        state,
                    } => {
                        let provider_name = if provider_name.is_empty() {
                            provider_id
                        } else {
                            provider_name
                        };
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(format!("{state:?}\0{destination}").as_bytes()),
                            turn_start: false,
                            kind: RowKind::KeyRequest {
                                provider_name: provider_name.clone().into(),
                                destination: destination.clone().into(),
                                state: *state,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::CompactionDivider {
                        id: part_id,
                        summary,
                        ..
                    } => {
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, summary);
                        rows.push(Row {
                            id: key.into(),
                            version: fnv1a(summary.as_bytes()),
                            turn_start: false,
                            kind: RowKind::CompactionDivider { summary: tree },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::PlanApproval {
                        id: part_id,
                        content,
                        state,
                        ..
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(
                                serde_json::to_vec(&state).unwrap_or_default().as_slice(),
                            ),
                            turn_start: false,
                            kind: RowKind::PlanApproval {
                                content: content.clone().into(),
                                state: state.clone(),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                }
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    // Timestamp strip under the entry's LAST row once the turn has settled
    // (chat-view.tsx: "No timestamp hover mid-stream"). The version bit keeps
    // the diff key honest for last-row kinds whose own version wouldn't
    // change when streaming flips off (chips).
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.copy_text = assistant_copy_text(entry);
        last.version ^= 1 << 62;
    }
    rows
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel. Tool groups get one larger global
/// step on either boundary so their dense chip stack has room to breathe.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return Theme::SPACE_LG;
    }
    let is_md = |k: &RowKind| matches!(k, RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. });
    let same_part_markdown = prev.is_some_and(|p| {
        is_md(&p.kind) && is_md(&row.kind) && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else if matches!(
        row.kind,
        RowKind::ToolGroup { .. }
            | RowKind::PlanApproval { .. }
            | RowKind::ModelProposal { .. }
            | RowKind::ProviderChoice { .. }
            | RowKind::QuestionCard { .. }
            | RowKind::KeyRequest { .. }
            | RowKind::TurnChangeCard { .. }
            | RowKind::CompactionDivider { .. }
    ) || prev.is_some_and(|row| {
        matches!(
            row.kind,
            RowKind::ToolGroup { .. }
                | RowKind::PlanApproval { .. }
                | RowKind::ModelProposal { .. }
                | RowKind::ProviderChoice { .. }
                | RowKind::QuestionCard { .. }
                | RowKind::KeyRequest { .. }
                | RowKind::TurnChangeCard { .. }
                | RowKind::CompactionDivider { .. }
        )
    }) {
        Theme::SPACE_MD
    } else {
        Theme::SPACE_SM
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}

/// The goal loop's band (ADR-goal-mode-verified-loop-at-turn-settle) concludes the Turn it caps: lift the
/// settled message's footer strip (timestamp + copy) down under the band, so
/// the block reads message → verdict → footer as one unit. The band entry's
/// own strip (its push time, nothing to copy) is replaced. The walk skips
/// footerless rows (a change-set card can sit between) and stops at a User
/// row, a loop row, or an earlier band — a band never claims a prompt's
/// footer, an earlier Turn's, or another band's; with no owner it keeps its
/// own strip. The band
/// also takes the owner's `entry_id`: the strip's hover reveal and copied
/// feedback key on it, so hovering the message lights the strip under the
/// band. Version bit 61 marks the handoff (bit 62 is the model's own footer
/// flag), so the row diff re-renders both affected rows. Pure over the
/// freshly rebuilt rows, so reapplying it every sync can't drift.
pub fn hand_footers_to_goal_bands(rows: &mut [Row]) {
    for ix in 0..rows.len() {
        if !matches!(rows[ix].kind, RowKind::GoalEnd { .. }) {
            continue;
        }
        let mut owner = None;
        for back in (0..ix).rev() {
            let row = &rows[back];
            if matches!(
                row.kind,
                RowKind::User { .. } | RowKind::GoalRound { .. } | RowKind::GoalEnd { .. }
            ) {
                break;
            }
            if row.timestamp.is_some() || row.copy_text.is_some() {
                owner = Some(back);
                break;
            }
        }
        let Some(owner) = owner else { continue };
        let timestamp = rows[owner].timestamp.take();
        let copy_text = rows[owner].copy_text.take();
        let entry_id = rows[owner].entry_id.clone();
        rows[owner].version ^= 1 << 61;
        let band = &mut rows[ix];
        band.entry_id = entry_id;
        band.timestamp = timestamp;
        band.copy_text = copy_text;
        band.version ^= 1 << 61;
    }
}

/// A loop row (ADR-goal-mode-verified-loop-at-turn-settle) opens its Turn right under the previous reply,
/// whose reserved footer lane would leave a hole above the light,
/// flush-left row. The row takes that footer over instead: the reply drops
/// its lane, the row draws its copy action inline at the right edge.
/// The walk skips footerless rows and may take a goal band's (already
/// handed-over) footer, but stops at a User row or another loop row. Runs
/// after [`hand_footers_to_goal_bands`]; version bit 61 marks the handoff.
pub fn hand_footers_to_goal_rounds(rows: &mut [Row]) {
    for ix in 0..rows.len() {
        if !matches!(rows[ix].kind, RowKind::GoalRound { .. }) {
            continue;
        }
        let mut owner = None;
        for back in (0..ix).rev() {
            let row = &rows[back];
            if matches!(row.kind, RowKind::User { .. } | RowKind::GoalRound { .. }) {
                break;
            }
            if row.timestamp.is_some() || row.copy_text.is_some() {
                owner = Some(back);
                break;
            }
        }
        let Some(owner) = owner else { continue };
        let timestamp = rows[owner].timestamp.take();
        let copy_text = rows[owner].copy_text.take();
        let entry_id = rows[owner].entry_id.clone();
        rows[owner].version ^= 1 << 61;
        let round = &mut rows[ix];
        round.timestamp = timestamp;
        round.copy_text = copy_text;
        round.version ^= 1 << 61;
        if let RowKind::GoalRound { footer_owner, .. } = &mut round.kind {
            *footer_owner = Some(entry_id);
        }
    }
}

/// The card row for one Turn's change set (ADR-turn-change-sets-and-review ticket 03). `entry_id`
/// is the last CONVERSATION entry the Turn rendered — the row the card
/// visually follows (trailing housekeeping System entries render below the
/// card). The version keys the row diff and moves only when the change
/// set's content does, so a live update resplices exactly the one card it
/// moved.
pub fn turn_change_row(
    turn_id: &str,
    entry_id: SharedString,
    change_set: &holt_proto::TurnChangeSet,
    restore: Option<crate::state::TurnRestoreMark>,
) -> Row {
    Row {
        id: SharedString::from(format!("{turn_id}#tcs")),
        version: turn_change_version(change_set, restore),
        turn_start: false,
        kind: RowKind::TurnChangeCard {
            change_set: Arc::new(change_set.clone()),
            restore,
        },
        entry_id,
        timestamp: None,
        copy_text: None,
    }
}

/// The live provider-retry chip row under a streaming entry. Keyed by entry
/// (the streaming reply it interrupts) with the attempt in the id, so each
/// scheduled retry replaces the previous chip row via the ordinary version
/// diff.
pub fn retry_chip_row(entry_id: SharedString, notice: &TurnRetryNotice) -> Row {
    Row {
        id: SharedString::from(format!("{entry_id}#retry{}", notice.attempt)),
        version: notice.retry_at_ms as u64,
        turn_start: false,
        kind: RowKind::RetryChip {
            attempt: notice.attempt,
            max_retries: notice.max_retries,
            retry_at_ms: notice.retry_at_ms,
            error: notice.error.clone().into(),
        },
        entry_id,
        timestamp: None,
        copy_text: None,
    }
}

/// Content fingerprint of one change set. Intra-session stability is all the
/// row diff needs; hashing the payload directly (rather than a map epoch)
/// keeps a live Turn's updates from resplicing every historical card.
fn turn_change_version(
    change_set: &holt_proto::TurnChangeSet,
    restore: Option<crate::state::TurnRestoreMark>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    restore.map(|mark| mark as u8).hash(&mut hasher);
    change_set.files.len().hash(&mut hasher);
    change_set.additions.hash(&mut hasher);
    change_set.deletions.hash(&mut hasher);
    change_set.truncated.hash(&mut hasher);
    std::mem::discriminant(&change_set.phase).hash(&mut hasher);
    for file in &change_set.files {
        file.path.hash(&mut hasher);
        file.old_path.hash(&mut hasher);
        std::mem::discriminant(&file.status).hash(&mut hasher);
        file.additions.hash(&mut hasher);
        file.deletions.hash(&mut hasher);
        file.binary.hash(&mut hasher);
    }
    hasher.finish()
}

pub(super) fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error,
            resolved,
            subagent_ref,
            subagent_status,
            subagent_tail,
            subagent_usage,
            gate,
            ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
            // Subagent lifecycle mutates a COMPLETED entry in place (eager-
            // done: the spawn resolves while the subagent runs on) and
            // `byte_len` above doesn't cover these fields — hash them or the
            // cached rows never refresh on status/tail changes.
            acc.push(
                subagent_ref.is_some() as u8
                    | match subagent_status {
                        None => 0,
                        Some(SubagentStatus::Running) => 1 << 1,
                        Some(SubagentStatus::Done) => 2 << 1,
                        Some(SubagentStatus::Failed) => 3 << 1,
                    },
            );
            if let Some(tail) = subagent_tail {
                acc.extend_from_slice(tail.as_bytes());
            }
            acc.extend_from_slice(&subagent_usage.unwrap_or_default().to_le_bytes());
            // The gate settles in place without touching resolved/is_error —
            // hashed via the shared helper (see `hash_gate`).
            hash_gate(&mut acc, gate.as_ref());
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
        // Cards settle in place, and `byte_len` only sees their state's
        // serialized LENGTH — `"pending"` and `"written"` are both 9 bytes, so
        // a written proposal kept rendering as pending. Hash the whole card.
        if matches!(
            part,
            MessagePart::PlanApproval { .. }
                | MessagePart::ModelProposal { .. }
                | MessagePart::ProviderChoice { .. }
                | MessagePart::QuestionCard { .. }
                | MessagePart::KeyRequest { .. }
        ) {
            acc.extend_from_slice(&serde_json::to_vec(part).unwrap_or_default());
        }
    }
    fnv1a(&acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::parse_full;
    use crate::transcript::tool::tool_group_summary;
    use holt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};

    fn parse(_: &str, text: &str) -> Arc<BlockTree> {
        Arc::new(parse_full(text))
    }

    fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: Some(status),
            continuation_of: None,
        }
    }

    #[test]
    fn a_proposal_card_settling_in_place_changes_the_fingerprint() {
        let card = |state| MessagePart::ModelProposal {
            id: "p1".into(),
            proposal_id: "prop".into(),
            targets: Vec::new(),
            summary: "add model".into(),
            lines: vec!["+ openai/gpt".into()],
            state,
        };
        let pending = assistant(
            "e1",
            MessageStatus::Complete,
            vec![card(holt_doc::ProposalCardState::Pending)],
        );
        let written = assistant(
            "e1",
            MessageStatus::Complete,
            vec![card(holt_doc::ProposalCardState::Written)],
        );
        // Same serialized state length — the old length-only hash collided.
        assert_ne!(
            entry_fingerprint(&pending, false),
            entry_fingerprint(&written, false)
        );
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn reasoning_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Reasoning {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn reasoning_joins_the_tool_group_accordion() {
        use crate::i18n::t;
        let _locale = crate::i18n::tests::locale_lock();
        // Thought → tool → thought → tool folds into ONE group row (user
        // request: the thought process lives inside the combined accordion),
        // and the collapsed summary names the thinking.
        let entry = assistant(
            "a1",
            MessageStatus::Complete,
            vec![
                reasoning_part("r0", "planning the first step"),
                tool_part("t1", "ls"),
                reasoning_part("r2", "now the second step"),
                tool_part("t3", "pwd"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one combined accordion row");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(tools.len(), 4);
        assert!(tools[0].is_thought && tools[2].is_thought);
        assert!(!tools[1].is_thought && !tools[3].is_thought);
        // Thought chips carry their text as a styled-line detail with an
        // ANALYTIC height, so the group's fold tween covers them.
        assert!(matches!(
            tools[0].detail.as_deref(),
            Some(ToolDetail::Thought { lines, .. }) if !lines.is_empty()
        ));
        let summary = tool_group_summary(tools);
        assert!(
            summary.starts_with(
                t!(
                    "conversation_transcript.tool.thought_times_with_base",
                    count = 2,
                    base = "Ran 2 commands"
                )
                .as_ref()
            ),
            "{summary}"
        );
        assert!(summary.contains("2 commands"), "{summary}");

        // A lone thought is still an accordion (with the group tween), named
        // plainly.
        let entry = assistant(
            "a2",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "just thinking")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(
            tool_group_summary(tools),
            t!("conversation_transcript.tool.thought_process").as_ref()
        );

        // Empty reasoning renders nothing.
        let entry = assistant(
            "a3",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }

    #[test]
    fn live_thought_streams_open_and_settles_closed() {
        let entry = assistant(
            "a1",
            MessageStatus::Streaming,
            vec![reasoning_part("r0", "thinking hard")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        // The live tail auto-opens the group; the chip is unresolved while
        // the part streams.
        assert!(*auto_open);
        assert!(!tools[0].resolved);

        let entry = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![
                reasoning_part("r0", "thinking hard"),
                text_part("t1", "answer"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert!(tools[0].resolved, "a followed thought is settled");
    }

    fn tool_part(id: &str, command: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            subagent_usage: None,
            gate: None,
        }
    }

    const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

    #[test]
    fn live_entry_splits_per_block_with_id_continuity() {
        // Live rows split per block exactly like completed ones (the list
        // virtualizes them — the fading tail is the only per-frame work).
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        assert_eq!(live_rows.len(), 3, "one live row per top-level block");
        assert!(
            live_rows
                .iter()
                .all(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
        );
        assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");
        assert_eq!(live_rows[2].id.as_ref(), "m1#t0.2");

        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        assert_eq!(done_rows.len(), 3, "three top-level blocks");
        // Every block row keeps its id across the flip — no flicker on handoff.
        for (live, done) in live_rows.iter().zip(&done_rows) {
            assert_eq!(live.id, done.id);
            // The flip changes the version even at identical text (the
            // streaming bit), forcing a splice.
            assert_ne!(live.version, done.version);
        }
        assert!(matches!(
            done_rows[0].kind,
            RowKind::Markdown { block_ix: 0, .. }
        ));
    }

    #[test]
    fn live_commit_changes_only_tail_row_versions() {
        // Streaming commit: appending to the last block leaves every settled
        // block row's (id, version) untouched — the diff splices only the tail.
        let t1 = "para one\n\npara two\n\npara three";
        let t2 = "para one\n\npara two\n\npara three grows here";
        let live1 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t1)]);
        let live2 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t2)]);
        let r1 = rows_for_entry(&live1, false, &mut parse);
        let r2 = rows_for_entry(&live2, false, &mut parse);
        assert_eq!(r1.len(), 3);
        assert_eq!(r2.len(), 3);
        assert_eq!(r1[0].version, r2[0].version, "settled block untouched");
        assert_eq!(r1[1].version, r2[1].version, "settled block untouched");
        assert_ne!(r1[2].version, r2[2].version, "tail block respliced");
        assert_eq!(diff_rows(&r1, &r2), Some((2..3, 1)));
    }

    #[test]
    fn split_sibling_gaps_match_live_internal_spacing() {
        // The live row spaces its internal blocks by MD_BLOCK_GAP; after the
        // live→split handoff the same boundaries are inter-row gaps. They must
        // be identical or the whole message jumps at completion.
        let done = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                text_part("t0", MD),
                tool_part("a", "ls"),
                text_part("t1", "tail para"),
            ],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        // Rows: t0.0, t0.1, t0.2 (three MD blocks), g0, t1.0.
        assert_eq!(rows.len(), 5);
        // Sibling markdown blocks from the same part: md block gap.
        assert_eq!(top_gap_for(Some(&rows[0]), &rows[1]), render::MD_BLOCK_GAP);
        assert_eq!(top_gap_for(Some(&rows[1]), &rows[2]), render::MD_BLOCK_GAP);
        // Markdown → tool group and tool group → next part: larger boundary.
        assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), Theme::SPACE_MD);
        assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), Theme::SPACE_MD);
        // Turn starts get the turn gap regardless.
        assert_eq!(top_gap_for(None, &rows[0]), Theme::SPACE_LG);
    }

    #[test]
    fn consecutive_tools_fold_into_groups_between_text() {
        let entry = assistant(
            "m2",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                text_part("t1", "after"),
                tool_part("c", "make"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["m2#t0.0", "m2#g0", "m2#t1.0", "m2#g1"]);
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!("group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(rows[0].turn_start && !rows[1].turn_start);
    }

    fn agent_part(id: &str, description: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: format!("Agent: {description}"),
                input: Some(serde_json::json!({ "description": description })),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: Some(format!("chat--sub--{id}")),
            subagent_status: Some(SubagentStatus::Running),
            subagent_tail: None,
            subagent_usage: None,
            gate: None,
        }
    }

    #[test]
    fn agent_calls_split_out_of_ordinary_tool_groups() {
        // Agent/spawn chips must not share a collapse with Reads/Runs: a
        // lone Agent used to hide behind "Called 1 tool", and a mixed
        // group hid the running subagent until the user opened the fold.
        let entry = assistant(
            "m-agent",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                agent_part("s1", "Map URL import ingest path"),
                tool_part("c", "make"),
                agent_part("s2", "Audit the fold path"),
                agent_part("s3", "Verify the commit cadence"),
                text_part("t1", "after"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(
            ids,
            [
                "m-agent#t0.0",
                "m-agent#g0",
                "m-agent#g1",
                "m-agent#g2",
                "m-agent#g3",
                "m-agent#t1.0",
            ]
        );

        let RowKind::ToolGroup { tools, auto_open } = &rows[1].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(tool_group_collapses(tools));
        assert!(!*auto_open);

        let RowKind::ToolGroup { tools, .. } = &rows[2].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_tool(&tools[0]));

        let RowKind::ToolGroup { tools, .. } = &rows[3].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(tool_group_collapses(tools));

        let RowKind::ToolGroup { tools, .. } = &rows[4].kind else {
            panic!("consecutive agents share a group")
        };
        assert_eq!(tools.len(), 2);
        assert!(!tool_group_collapses(tools));
        assert!(tools.iter().all(is_agent_tool));
    }

    #[test]
    fn a_fanout_call_is_drawn_by_its_item_parts_alone() {
        // The engine lands one display part per fan-out item right after
        // the `Agent` call; the container chip would repeat them.
        let entry = assistant(
            "m-fanout",
            MessageStatus::Complete,
            vec![
                agent_part("spawn-1", "audit {{item}}"),
                agent_part("spawn-1#item-0", "audit auth"),
                agent_part("spawn-1#item-1", "audit billing"),
                agent_part("spawn-2", "unrelated single spawn"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("agent group expected")
        };
        let ids: Vec<&str> = tools
            .iter()
            .map(|t| t.call_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, ["spawn-1#item-0", "spawn-1#item-1", "spawn-2"]);
    }

    fn phased(id: &str, label: &str, phase: &str) -> MessagePart {
        let mut part = agent_part(id, label);
        if let MessagePart::Tool { call, .. } = &mut part {
            *call = ToolCall::Unknown {
                name: format!("Agent: {label}"),
                input: Some(serde_json::json!({ "phase": phase })),
            };
        }
        part
    }

    #[test]
    fn a_workflow_is_its_own_row_with_one_agent_row_per_phase() {
        let mut container = tool_part("wf-1", "ignored");
        if let MessagePart::Tool {
            call,
            subagent_status,
            subagent_usage,
            ..
        } = &mut container
        {
            *call = ToolCall::Unknown {
                name: "Workflow: audit".into(),
                input: None,
            };
            *subagent_status = Some(SubagentStatus::Running);
            *subagent_usage = Some(1200);
        }
        let entry = assistant(
            "m-wf",
            MessageStatus::Complete,
            vec![
                container,
                phased("wf-1#item-0", "a", "Find"),
                phased("wf-1#item-1", "b", "Find"),
                phased("wf-1#item-2", "verify", "Verify"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 3);
        let RowKind::Workflow {
            call_id,
            name,
            status,
            tokens,
        } = &rows[0].kind
        else {
            panic!("workflow row expected")
        };
        assert_eq!(call_id.as_ref(), "wf-1");
        assert_eq!(name.as_ref(), "audit");
        assert_eq!(*status, Some(SubagentStatus::Running));
        assert_eq!(*tokens, Some(1200));
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!("phase row expected")
        };
        assert_eq!(tools.len(), 2);
        assert_eq!(tool_phase(&tools[0].call), Some("Find"));
        let RowKind::ToolGroup { tools, .. } = &rows[2].kind else {
            panic!("phase row expected")
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tool_phase(&tools[0].call), Some("Verify"));
    }

    #[test]
    fn a_workflow_report_draws_as_a_light_row() {
        let _locale = crate::i18n::tests::locale_lock();
        let entry = SessionMessageEntry {
            id: "m-report".into(),
            role: MessageRole::User,
            parts: vec![
                text_part("t0", "Workflow \"audit\" (run x) finished. …"),
                MessagePart::WorkflowResult {
                    id: "w0".into(),
                    name: "audit".into(),
                    failed: false,
                },
            ],
            created_at: 0,
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::GoalRound { label, prompt, .. } = &rows[0].kind else {
            panic!("report row expected, not a user bubble")
        };
        assert_eq!(label.as_ref(), "Workflow “audit” finished");
        assert!(prompt.starts_with("Workflow \"audit\" (run x)"));
    }

    #[test]
    fn stray_subagent_ref_on_a_run_chip_stays_an_ordinary_tool() {
        // Docs written before the claude-driver fix carry subagent refs on
        // ordinary Run chips (a background shell's task_notification was
        // mis-tagged as subagent traffic). The ref alone must not change the
        // chip's genus: it folds with its neighbors and renders as a plain
        // tool, never as a spawn link to a doc that was never created.
        let mut stray = tool_part("b", "git clone …");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut stray
        {
            *subagent_ref = Some("chat--sub--b".into());
            *subagent_status = Some(SubagentStatus::Done);
        }
        let entry = assistant(
            "m-stray",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), stray, tool_part("c", "make")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one folded group, no agent split");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("tool group expected")
        };
        assert_eq!(tools.len(), 3);
        assert!(tool_group_collapses(tools));
        assert!(tools.iter().all(|t| !is_agent_tool(t)));
        assert!(tools.iter().all(|t| !is_spawn_link(t)));
    }

    #[test]
    fn lone_completed_agent_stays_uncollapsed() {
        let entry = assistant(
            "m-lone",
            MessageStatus::Complete,
            vec![agent_part("s1", "scan repo")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools), "no 'Called 1 tool' wrap");
        assert!(
            !*auto_open,
            "auto_open is a streaming flag; agent rows ignore it at paint"
        );
    }

    #[test]
    fn completed_spawn_keeps_its_usage_summary_for_rendering() {
        let mut entry = assistant(
            "m-usage",
            MessageStatus::Complete,
            vec![agent_part("s1", "scan repo")],
        );
        if let MessagePart::Tool {
            subagent_usage,
            subagent_status,
            ..
        } = &mut entry.parts[0]
        {
            *subagent_usage = Some(12_300);
            *subagent_status = Some(SubagentStatus::Done);
        }
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools[0].subagent_usage, Some(12_300));
    }

    #[test]
    fn pre_spawn_agent_name_is_enough_to_split() {
        // Before the engine stamps subagent_ref the chip is already named
        // "Agent: …" — that genus must split, or the spawn hides until the
        // first tagged event.
        let mut part = agent_part("s1", "scan repo");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut part
        {
            *subagent_ref = None;
            *subagent_status = None;
        }
        let entry = assistant(
            "m-pre",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), part],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 2);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!()
        };
        assert!(tool_group_collapses(tools));
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!()
        };
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_call(&tools[0].call));
    }

    #[test]
    fn trailing_group_auto_opens_only_while_streaming() {
        let parts = vec![text_part("t0", "hi"), tool_part("a", "ls")];
        let streaming = assistant("m3", MessageStatus::Streaming, parts.clone());
        let rows = rows_for_entry(&streaming, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(auto_open, "trailing group opens while streaming");

        let complete = assistant("m3", MessageStatus::Complete, parts);
        let rows = rows_for_entry(&complete, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(!auto_open);

        // A non-trailing group never auto-opens.
        let mid = assistant(
            "m4",
            MessageStatus::Streaming,
            vec![tool_part("a", "ls"), text_part("t0", "hi")],
        );
        let rows = rows_for_entry(&mid, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[0].kind else {
            panic!()
        };
        assert!(!auto_open);
    }

    #[test]
    fn user_rows_and_echo_versions() {
        let mut entry = assistant("u1", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", "hello")];
        let confirmed = rows_for_entry(&entry, false, &mut parse);
        let echoed = rows_for_entry(&entry, true, &mut parse);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].id, echoed[0].id);
        // Pending → confirmed changes the version so the row re-renders.
        assert_ne!(confirmed[0].version, echoed[0].version);
        assert!(matches!(
            &echoed[0].kind,
            RowKind::User { pending: true, .. }
        ));
    }

    /// A `/goal` arming send chips the directive inline at the bubble's head
    /// instead of lifting a pill above it: the marker block leaves the text
    /// entirely, the row flags the chip, and the copy strip stays the bare
    /// objective.
    #[test]
    fn user_rows_chip_the_goal_directive_inline() {
        let mut entry = assistant("u9", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part(
            "t0",
            &crate::goal::with_goal_block("ship the login page"),
        )];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User {
            text,
            raw,
            badges,
            goal_chip,
            ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert!(goal_chip, "the row chips the goal directive inline");
        assert!(badges.is_empty(), "no pill rides above the bubble");
        assert_eq!(text.as_ref(), "ship the login page");
        assert_eq!(raw.as_ref(), "ship the login page");
        assert_eq!(rows[0].copy_text.as_deref(), Some("ship the login page"));
        assert!(!text.contains("Goal armed"));

        // The composer appends the path list AFTER the goal marker: the
        // trailers still peel, so the bubble neither leaks the marker
        // sentence nor loses the chip.
        let with_refs = crate::path_refs::append_references(
            &crate::goal::with_goal_block("ship the login page"),
            &[crate::path_refs::PathRef {
                id: "r0".into(),
                path: "/repo/src/login.rs".into(),
                is_dir: false,
                managed: false,
            }],
        );
        entry.parts = vec![text_part("t0", &with_refs)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text, goal_chip, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert!(goal_chip, "the chip survives an appended path list");
        assert_eq!(text.as_ref(), "ship the login page");

        // An ordinary prompt never chips.
        entry.parts = vec![text_part("t0", "just a prompt")];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { goal_chip, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert!(!goal_chip);
    }

    #[test]
    fn user_rows_split_attachment_refs_from_text() {
        let content = crate::attachments::with_attachments(
            "what color is this?",
            &["/data/uploads/ab12-red.png".to_string()],
        );
        let mut entry = assistant("u2", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &content)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "what color is this?");
        assert_eq!(rows[0].copy_text.as_deref(), Some("what color is this?"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, "/data/uploads/ab12-red.png");
        assert_eq!(attachments[0].name, "ab12-red.png");

        // Image-only send: no bubble text, refs parsed.
        let only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
        entry.parts = vec![text_part("t0", &only)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "");
        assert!(rows[0].copy_text.is_none());
        assert_eq!(attachments.len(), 1);
    }

    /// A sent prompt's file mentions render as chips in the transcript: the
    /// row carries the projected display text plus spans, while ordinary
    /// prompts keep the empty-spans fast path. The row version derives from
    /// the RAW text either way, so projection never perturbs the diff key.
    #[test]
    fn user_rows_project_file_mentions_into_chips() {
        let raw = "look at [composer.rs](holt-file:crates/ui/src/composer.rs) please";
        let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", raw)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert!(
            !text.contains("holt-file:"),
            "raw link left visible: {text}"
        );
        assert!(text.contains("composer.rs"));
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].is_dir);
        assert_eq!(mentions[0].path.as_ref(), "crates/ui/src/composer.rs");
        assert_eq!(&text[mentions[0].range.clone()], {
            let projected: &str = "\u{00A0}@composer.rs\u{00A0}";
            projected
        });
        assert_eq!(rows[0].version, (raw.len() as u64) << 1);

        entry.parts = vec![text_part("t0", "no mentions here")];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "no mentions here");
        assert!(mentions.is_empty());
    }

    #[test]
    fn format_skill_title_converts_kebab_and_snake_case() {
        assert_eq!(format_skill_title("ask-matt"), "Ask Matt");
        assert_eq!(format_skill_title("code-review"), "Code Review");
        assert_eq!(format_skill_title("triage"), "Triage");
        assert_eq!(format_skill_title("grill_with_docs"), "Grill With Docs");
        assert_eq!(
            format_skill_title("retro-manga-graphic-logo"),
            "Retro Manga Graphic Logo"
        );
    }

    #[test]
    fn skill_file_display_collapses_the_host_home_and_strips_the_scheme() {
        let home = home_dir().expect("a home directory resolves in the test env");
        assert_eq!(
            skill_file_display(&format!("{home}/.agents/skills/ask-matt/SKILL.md")),
            "~/.agents/skills/ask-matt/SKILL.md"
        );
        assert_eq!(
            skill_file_display(&format!("file://{home}/SKILL.md")),
            "~/SKILL.md"
        );
        assert_eq!(
            skill_file_display("/opt/skills/ask-matt/SKILL.md"),
            "/opt/skills/ask-matt/SKILL.md"
        );
    }

    #[test]
    fn skill_file_display_folds_separators_only_on_windows() {
        // A backslash is a filename character on unix and a separator on
        // Windows: the same chip reads differently by design.
        assert_eq!(
            skill_file_display_on(
                false,
                "/home/onion",
                "/home/onion/.agents/skills/back\\slash/SKILL.md"
            ),
            "~/.agents/skills/back\\slash/SKILL.md"
        );
        assert_eq!(
            skill_file_display_on(
                true,
                r"C:\Users\onion",
                r"C:\Users\onion\.agents\skills\back\slash\SKILL.md"
            ),
            "~/.agents/skills/back/slash/SKILL.md"
        );
    }

    #[test]
    fn skill_file_display_collapses_whole_components_only() {
        // `/home/onion2` shares the string prefix with `/home/onion` but is
        // not under it, so it keeps its spelling (and its separators).
        assert_eq!(
            skill_file_display_on(
                false,
                "/home/onion",
                "/home/onion2/.agents/skills/x/SKILL.md"
            ),
            "/home/onion2/.agents/skills/x/SKILL.md"
        );
        assert_eq!(
            skill_file_display_on(
                true,
                r"C:\Users\onion",
                r"C:\Users\onion2\.agents\skills\x\SKILL.md"
            ),
            "C:/Users/onion2/.agents/skills/x/SKILL.md"
        );
        // A trailing separator on home still collapses.
        assert_eq!(
            skill_file_display_on(false, "/home/onion/", "/home/onion/a.md"),
            "~/a.md"
        );
    }

    #[test]
    fn user_rows_with_skill_invocation_render_in_single_bubble() {
        let mut entry = assistant("u4", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![
            MessagePart::Skill {
                id: "s0".into(),
                name: "ask-matt".into(),
                file: "/home/.agents/skills/ask-matt/SKILL.md".into(),
                content: Some("<skill name=\"ask-matt\">…</skill>".into()),
            },
            text_part("t0", "你好吗"),
        ];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "should produce a single user row");
        let RowKind::User {
            text,
            skill,
            mentions,
            attachments,
            ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "你好吗");
        assert!(mentions.is_empty());
        assert!(attachments.is_empty());
        let skill = skill.as_ref().expect("expected skill metadata");
        assert_eq!(skill.name.as_ref(), "ask-matt");
        assert_eq!(
            skill.file.as_ref(),
            "/home/.agents/skills/ask-matt/SKILL.md"
        );
        assert_eq!(rows[0].copy_text.as_deref(), Some("/skill ask-matt 你好吗"));

        // Skill invocation without extra text produces a single user row
        entry.parts = vec![MessagePart::Skill {
            id: "s0".into(),
            name: "ask-matt".into(),
            file: "/home/.agents/skills/ask-matt/SKILL.md".into(),
            content: None,
        }];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User { text, skill, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "");
        assert!(skill.is_some());
        assert_eq!(rows[0].copy_text.as_deref(), Some("/skill ask-matt"));
    }

    /// The History notice (ADR-history-is-a-separate-persisted-record) renders as one quiet full-width row —
    /// not an error chip, not markdown — with the engine's prose intact.
    #[test]
    fn a_notice_part_renders_one_quiet_row() {
        let mut entry = assistant(
            "s1",
            MessageStatus::Complete,
            vec![MessagePart::Notice {
                id: "n0".into(),
                message: "Everything above this line is visible to you, but the model starts fresh after it.".into(),
                goal: None,
            }],
        );
        entry.role = MessageRole::System;
        entry.status = None;
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            RowKind::Notice { message } => assert!(message.contains("model starts fresh")),
            _other => panic!("expected a notice row"),
        }
        assert!(rows[0].copy_text.is_none());
    }

    /// The goal loop's lifecycle Notice (ADR-goal-mode-verified-loop-at-turn-settle) carries its transition
    /// tag and renders as the band row, not a plain housekeeping row.
    #[test]
    fn a_goal_notice_renders_the_goal_end_band() {
        let mut entry = assistant(
            "goal-abc",
            MessageStatus::Complete,
            vec![MessagePart::Notice {
                id: "n0".into(),
                message: "Goal achieved · 1 round · 12 min · 45.2k tokens".into(),
                goal: Some(holt_proto::GoalEvent::Achieved),
            }],
        );
        entry.role = MessageRole::System;
        entry.status = None;
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            RowKind::GoalEnd { message, event } => {
                assert!(message.contains("Goal achieved"));
                assert_eq!(*event, holt_proto::GoalEvent::Achieved);
            }
            _other => panic!("expected the goal-end band row"),
        }
        // The band keys on the part's tag, never the entry id or the text: a
        // `goal-*` notice without a tag (pre-tag history) stays plain.
        entry.parts = vec![MessagePart::Notice {
            id: "n0".into(),
            message: "Goal achieved · 1 round".into(),
            goal: None,
        }];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert!(matches!(rows[0].kind, RowKind::Notice { .. }));
    }

    fn goal_rows(id: &str, created_at: i64) -> Vec<Row> {
        let mut entry = assistant(
            id,
            MessageStatus::Complete,
            vec![MessagePart::Notice {
                id: "n0".into(),
                message: "Goal achieved · 12 min".into(),
                goal: Some(holt_proto::GoalEvent::Achieved),
            }],
        );
        entry.role = MessageRole::System;
        entry.status = None;
        entry.created_at = created_at;
        rows_for_entry(&entry, false, &mut parse)
    }

    fn round_rows(
        id: &str,
        round: holt_proto::GoalRoundKind,
        reason: &str,
        created_at: i64,
    ) -> Vec<Row> {
        let mut entry = assistant(
            id,
            MessageStatus::Complete,
            vec![
                text_part("t0", "Keep working toward the goal stated earlier."),
                MessagePart::GoalRound {
                    id: "g0".into(),
                    round,
                    reason: reason.into(),
                },
            ],
        );
        entry.role = MessageRole::User;
        entry.status = None;
        entry.created_at = created_at;
        rows_for_entry(&entry, false, &mut parse)
    }

    fn user_rows(id: &str, text: &str, created_at: i64) -> Vec<Row> {
        let mut entry = assistant(id, MessageStatus::Complete, vec![text_part("t0", text)]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.created_at = created_at;
        rows_for_entry(&entry, false, &mut parse)
    }

    fn reply_rows(id: &str, text: &str, created_at: i64) -> Vec<Row> {
        let mut entry = assistant(id, MessageStatus::Complete, vec![text_part("t0", text)]);
        entry.created_at = created_at;
        rows_for_entry(&entry, false, &mut parse)
    }

    /// A loop-queued continuation (ADR-goal-mode-verified-loop-at-turn-settle) is the loop speaking: it draws
    /// as the loop row — round label plus the verifier's missing piece, the
    /// full prompt kept for the fold — never as a user bubble.
    #[test]
    fn goal_continuations_render_as_loop_rows() {
        let rows = round_rows(
            "c1",
            holt_proto::GoalRoundKind::Continue(2),
            "tests not run",
            5,
        );
        let prompt = "Keep working toward the goal stated earlier.";
        assert_eq!(rows.len(), 1);
        let RowKind::GoalRound {
            label,
            reason,
            prompt: kept,
            ..
        } = &rows[0].kind
        else {
            panic!("expected the loop row");
        };
        assert_eq!(label.as_ref(), "Round 2");
        assert_eq!(reason.as_ref(), "tests not run");
        assert_eq!(kept.as_ref(), prompt);
        assert!(rows[0].turn_start);
        assert!(rows[0].timestamp.is_none() && rows[0].copy_text.is_none());
    }

    #[test]
    fn a_goal_round_takes_the_previous_reply_footer() {
        let mut rows = user_rows("u1", "fix it", 1);
        rows.extend(reply_rows("m1", "done", 2));
        rows.extend(round_rows("c1", holt_proto::GoalRoundKind::Resume, "", 3));
        rows.extend(reply_rows("m2", "more", 4));
        let before = rows.clone();

        hand_footers_to_goal_rounds(&mut rows);

        let (reply, loop_row) = (&rows[1], &rows[2]);
        assert!(reply.timestamp.is_none() && reply.copy_text.is_none());
        assert_eq!(loop_row.timestamp, Some(2));
        assert_eq!(loop_row.copy_text.as_deref(), Some("done"));
        // The row keeps its own entry id (it anchors its Turn); the owner
        // rides the kind.
        assert_eq!(loop_row.entry_id.as_ref(), "c1");
        assert!(matches!(
            &loop_row.kind,
            RowKind::GoalRound { footer_owner: Some(owner), .. } if owner.as_ref() == "m1"
        ));
        assert_eq!(reply.version, before[1].version ^ 1 << 61);
        assert_eq!(loop_row.version, before[2].version ^ 1 << 61);
        // The reply after the loop row keeps its own strip.
        assert_eq!(rows[3].timestamp, Some(4));
    }

    #[test]
    fn a_goal_round_right_after_a_prompt_takes_nothing() {
        let mut rows = user_rows("u1", "fix it", 1);
        rows.extend(round_rows(
            "c1",
            holt_proto::GoalRoundKind::Continue(1),
            "x",
            3,
        ));
        let before = rows.clone();

        hand_footers_to_goal_rounds(&mut rows);

        assert_eq!(rows[0].timestamp, Some(1));
        assert!(rows[1].timestamp.is_none());
        assert_eq!(rows[0].version, before[0].version);
        assert_eq!(rows[1].version, before[1].version);
    }

    #[test]
    fn a_goal_band_takes_the_settled_reply_footer() {
        let mut rows = user_rows("u1", "fix it", 1);
        rows.extend(reply_rows("m1", "done", 2));
        // A footerless row between (a change-set card's shape) is skipped.
        rows.push(Row {
            id: "turn1#tcs".into(),
            version: 7,
            turn_start: false,
            kind: RowKind::Notice {
                message: "card".into(),
            },
            entry_id: "m1".into(),
            timestamp: None,
            copy_text: None,
        });
        rows.extend(goal_rows("goal-a", 3));
        let before = rows.clone();

        hand_footers_to_goal_bands(&mut rows);

        let (reply, band) = (&rows[1], &rows[3]);
        assert!(reply.timestamp.is_none() && reply.copy_text.is_none());
        assert_eq!(band.timestamp, Some(2));
        assert_eq!(band.copy_text.as_deref(), Some("done"));
        // One hover unit with the message it caps.
        assert_eq!(band.entry_id.as_ref(), "m1");
        assert_eq!(reply.version, before[1].version ^ 1 << 61);
        assert_eq!(band.version, before[3].version ^ 1 << 61);
        // The prompt and the card are untouched.
        assert_eq!(rows[0].timestamp, Some(1));
        assert_eq!(rows[2].version, before[2].version);
    }

    #[test]
    fn a_goal_band_right_after_the_prompt_keeps_its_own_footer() {
        // "Goal cleared — the turn could not start": no reply to own.
        let mut rows = user_rows("u1", "fix it", 1);
        rows.extend(goal_rows("goal-a", 3));
        let before = rows.clone();

        hand_footers_to_goal_bands(&mut rows);

        assert_eq!(rows[0].timestamp, Some(1));
        assert_eq!(rows[0].copy_text.as_deref(), Some("fix it"));
        assert_eq!(rows[1].timestamp, Some(3));
        assert_eq!(rows[1].entry_id.as_ref(), "goal-a");
        assert_eq!(rows[0].version, before[0].version);
        assert_eq!(rows[1].version, before[1].version);
    }

    #[test]
    fn adjacent_goal_bands_never_take_each_others_footer() {
        let mut rows = user_rows("u1", "fix it", 1);
        rows.extend(reply_rows("m1", "done", 2));
        rows.extend(goal_rows("goal-a", 3));
        rows.extend(goal_rows("goal-b", 4));

        hand_footers_to_goal_bands(&mut rows);

        assert_eq!(rows[2].timestamp, Some(2));
        assert_eq!(rows[2].copy_text.as_deref(), Some("done"));
        assert_eq!(rows[3].timestamp, Some(4));
        assert!(rows[3].copy_text.is_none());
        assert_eq!(rows[3].entry_id.as_ref(), "goal-b");
    }

    /// The Compaction divider (ADR-compaction-is-not-a-turn) renders as one quiet row carrying
    /// the summary — the expansion state is the
    /// view's, not the model's (collapsed is only the default).
    #[test]
    fn a_compaction_divider_part_renders_one_row_with_the_summary() {
        let entry = assistant(
            "s2",
            MessageStatus::Complete,
            vec![MessagePart::CompactionDivider {
                id: "d0".into(),
                summary: "## Goal\nship the compaction slice".into(),
                tokens_before: 45231,
                tokens_after: 8002,
                trigger: holt_doc::parts::CompactionTrigger::Automatic,
                timestamp: 7,
            }],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            RowKind::CompactionDivider { summary } => {
                assert!(matches!(
                    summary.blocks.first().map(|top| &top.block),
                    Some(crate::markdown::parser::Block::Heading { level: 2, .. })
                ));
                assert_eq!(summary.blocks.len(), 2);
            }
            _other => panic!("expected a compaction divider row"),
        }
    }

    #[test]
    fn invocation_chip_opens_the_agent_entry_before_thinking() {
        // An invocation-seeded agent entry: the Skill chip (with its
        // block) leads, thinking follows, text closes.
        let entry = assistant(
            "a1",
            MessageStatus::Complete,
            vec![
                MessagePart::Skill {
                    id: "s0".into(),
                    name: "ask-matt".into(),
                    file: "/home/.agents/skills/ask-matt/SKILL.md".into(),
                    content: Some("<skill name=\"ask-matt\">body</skill>".into()),
                },
                MessagePart::Reasoning {
                    id: "r1".into(),
                    text: "thinking…".into(),
                },
                text_part("t2", "answer"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        match &rows[0].kind {
            RowKind::SkillChip { name, content, .. } => {
                assert_eq!(name.as_ref(), "ask-matt");
                assert_eq!(
                    content.as_deref(),
                    Some("<skill name=\"ask-matt\">body</skill>")
                );
            }
            _other => panic!("expected the invocation chip first"),
        }
        assert!(matches!(rows[1].kind, RowKind::ToolGroup { .. }));
        assert!(matches!(rows[2].kind, RowKind::Markdown { .. }));

        // A skill-file read joins the thought/tool group instead of
        // producing another standalone skill bubble.
        let mut plain = entry.clone();
        plain.parts[0] = MessagePart::Skill {
            id: "s0".into(),
            name: "ask-matt".into(),
            file: "/home/.agents/skills/ask-matt/SKILL.md".into(),
            content: None,
        };
        let plain_rows = rows_for_entry(&plain, false, &mut parse);
        assert_eq!(plain_rows.len(), 2);
        let RowKind::ToolGroup { tools, .. } = &plain_rows[0].kind else {
            panic!("expected the read and thought in one tool group");
        };
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0].call,
            ToolCall::ReadFile {
                path: "/home/.agents/skills/ask-matt/SKILL.md".into()
            }
        );
        assert!(tools[1].is_thought);
        assert!(matches!(plain_rows[1].kind, RowKind::Markdown { .. }));
    }

    #[test]
    fn diff_rows_appends_and_middle_edits() {
        let entry1 = assistant("m1", MessageStatus::Complete, vec![text_part("t0", "one")]);
        let entry2 = assistant("m2", MessageStatus::Complete, vec![text_part("t0", "two")]);
        let r1 = rows_for_entry(&entry1, false, &mut parse);
        let mut both = r1.clone();
        both.extend(rows_for_entry(&entry2, false, &mut parse));

        // Identical → None.
        assert!(diff_rows(&r1, &r1.clone()).is_none());
        // Append → splice at the tail.
        assert_eq!(diff_rows(&r1, &both), Some((1..1, 1)));
        // Removal from the end.
        assert_eq!(diff_rows(&both, &r1), Some((1..2, 0)));

        // Middle content change: only the changed row splices.
        let entry1b = assistant(
            "m1",
            MessageStatus::Complete,
            vec![text_part("t0", "one more")],
        );
        let mut both_b = rows_for_entry(&entry1b, false, &mut parse);
        both_b.extend(rows_for_entry(&entry2, false, &mut parse));
        assert_eq!(diff_rows(&both, &both_b), Some((0..1, 1)));

        // Full reset when everything shifts.
        let r2 = rows_for_entry(&entry2, false, &mut parse);
        assert_eq!(diff_rows(&r1, &r2), Some((0..1, 1)));
    }

    #[test]
    fn diff_handles_live_to_split_growth() {
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        // Same ids; every version flips its streaming bit → one 3-row splice.
        assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..3, 3)));
    }

    #[test]
    fn timestamp_strip_lands_on_the_last_settled_row() {
        use chrono::FixedOffset;
        // Fixed zone (UTC−4): "Jul 1, 3:45 PM" — the exact formatTimestamp
        // shape (short month, numeric day, no leading zero, 2-digit minutes).
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let ms = chrono::DateTime::parse_from_rfc3339("2026-07-01T19:45:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(format_timestamp(ms, &tz), "Jul 1, 3:45 PM");

        // User entries carry the strip on their single row (pending too).
        let user = SessionMessageEntry {
            id: "u1".into(),
            role: MessageRole::User,
            parts: vec![text_part("p1", "hi")],
            created_at: ms,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        };
        let rows = rows_for_entry(&user, true, &mut parse);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, Some(ms));

        // Assistant entries: strip on the LAST row once settled…
        let done = assistant(
            "a1",
            MessageStatus::Complete,
            vec![text_part("p1", "one\n\ntwo")],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        assert!(rows.len() >= 2);
        assert_eq!(rows.last().unwrap().timestamp, Some(done.created_at));
        assert_eq!(
            rows.last().unwrap().copy_text.as_deref(),
            Some("one\n\ntwo")
        );
        assert!(rows[..rows.len() - 1].iter().all(|r| r.timestamp.is_none()));
        assert!(rows[..rows.len() - 1].iter().all(|r| r.copy_text.is_none()));

        // …but never mid-stream (chat-view.tsx: no hover under a moving reply).
        let live = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![text_part("p1", "streaming…")],
        );
        let rows = rows_for_entry(&live, false, &mut parse);
        assert!(rows.iter().all(|r| r.timestamp.is_none()));
        assert!(rows.iter().all(|r| r.copy_text.is_none()));
        // Every row knows its entry (the hover group).
        assert!(rows.iter().all(|r| r.entry_id.as_ref() == live.id));
    }

    #[test]
    fn message_copy_keeps_authored_text_and_excludes_tool_traces() {
        let entry = assistant(
            "a-copy",
            MessageStatus::Complete,
            vec![
                text_part("p1", "First **paragraph**."),
                tool_part("tool", "printf hidden"),
                text_part("p2", "    indented code\n    stays indented"),
            ],
        );
        assert_eq!(
            assistant_copy_text(&entry).as_deref(),
            Some("First **paragraph**.\n\n    indented code\n    stays indented")
        );
    }

    #[test]
    fn empty_text_parts_produce_no_rows() {
        let entry = assistant(
            "m9",
            MessageStatus::Streaming,
            vec![text_part("t0", ""), text_part("t1", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }

    /// The retry chip row keys per attempt under the streaming entry and
    /// versions by retry time, so each scheduled retry resplices the chip.
    #[test]
    fn retry_chip_row_carries_the_notice_and_versions_its_content() {
        let notice = holt_rpc::retries::TurnRetryNotice {
            chat_id: "chat-1".into(),
            attempt: 2,
            max_retries: 5,
            delay_ms: 1500,
            retry_at_ms: 1234,
            error: "connection reset by peer".into(),
        };
        let row = retry_chip_row("m-9".into(), &notice);
        assert_eq!(row.id, "m-9#retry2");
        assert_eq!(row.entry_id, "m-9");
        assert!(!row.turn_start);
        assert_eq!(row.version, 1234);
        match row.kind {
            RowKind::RetryChip {
                attempt,
                max_retries,
                retry_at_ms,
                error,
            } => {
                assert_eq!(attempt, 2);
                assert_eq!(max_retries, 5);
                // The chip counts down from this stamp, never from the
                // attempt's total backoff.
                assert_eq!(retry_at_ms, 1234);
                assert_eq!(&*error, "connection reset by peer");
            }
            _ => panic!("wrong row kind"),
        }
    }

    /// A card row carries the Turn's change set, never opens a turn gap, and
    /// sits under the entry it follows — while its version moves ONLY with
    /// the change set's content, so a live frame resplices exactly its card.
    #[test]
    fn turn_change_row_carries_the_set_and_versions_its_content() {
        let file = holt_proto::TurnFileChange {
            path: "src/lib.rs".into(),
            old_path: None,
            status: holt_proto::TurnFileChangeStatus::Modified,
            additions: 3,
            deletions: 1,
            binary: false,
        };
        let live = change_set(
            "m-1",
            holt_proto::TurnChangeSetPhase::Live,
            vec![file.clone()],
        );
        let row = turn_change_row("m-1", "a-1".into(), &live, None);
        assert_eq!(row.id.as_ref(), "m-1#tcs");
        assert_eq!(row.entry_id.as_ref(), "a-1");
        assert!(!row.turn_start, "the card closes a turn, never opens one");
        assert!(row.timestamp.is_none() && row.copy_text.is_none());
        let RowKind::TurnChangeCard {
            change_set: stored, ..
        } = &row.kind
        else {
            panic!("expected a change-card row")
        };
        assert_eq!(stored.message_id, "m-1");
        assert_eq!(stored.files, vec![file.clone()]);

        // Same payload (phase flip aside): a content change moves the
        // version, an identical rebuild does not.
        let same_live = turn_change_row("m-1", "a-1".into(), &live, None);
        assert_eq!(same_live.version, row.version);
        let final_set = change_set(
            "m-1",
            holt_proto::TurnChangeSetPhase::Final,
            vec![holt_proto::TurnFileChange {
                additions: 4,
                ..file.clone()
            }],
        );
        let settled = turn_change_row("m-1", "a-1".into(), &final_set, None);
        assert_ne!(settled.version, row.version);
        // A different Turn's card has its own identity.
        let other = turn_change_row("m-2", "a-1".into(), &final_set, None);
        assert_ne!(other.id, row.id);
        // A restore mark redraws the same card.
        let restored = turn_change_row(
            "m-1",
            "a-1".into(),
            &final_set,
            Some(crate::state::TurnRestoreMark::Restored),
        );
        assert_ne!(restored.version, settled.version);
    }

    fn change_set(
        message_id: &str,
        phase: holt_proto::TurnChangeSetPhase,
        files: Vec<holt_proto::TurnFileChange>,
    ) -> holt_proto::TurnChangeSet {
        holt_proto::TurnChangeSet {
            chat_id: "chat-1".into(),
            message_id: message_id.into(),
            phase,
            additions: files.iter().map(|file| file.additions).sum(),
            deletions: files.iter().map(|file| file.deletions).sum(),
            truncated: false,
            updated_at: chrono::Utc::now(),
            files,
        }
    }
}
