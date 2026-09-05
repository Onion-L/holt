//! The single-agent run loop over pi-core: per-chat runtime state,
//! event-to-transcript translation, and history persistence.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, sanitize_tool_call};
use holt_proto::{
    Chat, PermissionMode, ReasoningLevel, Session, SessionStatus, ToolCall as TranscriptToolCall,
};
use pi_core::agent::harness::messages::convert_to_llm as harness_convert_to_llm;
use pi_core::{
    agent::{
        agent_loop::{AgentEventSink, run_agent_loop},
        types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentToolResult},
    },
    ai::{
        compat,
        types::{
            AssistantContent, BlockContent, Context as PiContext, Model as PiModel, RoleUser,
            SimpleStreamOptions, ThinkingLevel as ProviderThinkingLevel, UserContent, UserMessage,
        },
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::history::CompactionRecord;
use crate::store::{delete_transcript, load_transcript, persist_transcript};

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("system_prompt.md");

/// Transcript entry id of the where-the-model's-memory-begins notice
/// (ADR-0010): appended once when a legacy or damaged History is found,
/// and the guard that keeps it from being written twice.
const HISTORY_NOTICE_ENTRY_ID: &str = "history-notice";

fn system_prompt(cwd: &str) -> String {
    SYSTEM_PROMPT_TEMPLATE.replace("{{cwd}}", cwd)
}

/// The run's system prompt plus the catalog it was built from: the
/// coding-agent template with the metadata-only skill block appended
/// (ADR-0006), from a fresh scan of the chat's three roots — so skills
/// added, edited, or removed since the last turn are already reflected.
/// The catalog rides along: it is also what collapses reads of a skill's
/// `SKILL.md` into chips while decoding tool calls for the transcript.
async fn run_system_prompt(
    skills: &crate::skills::Skills,
    cwd: &str,
) -> (String, crate::skills::Catalog) {
    let catalog = skills.catalog(Some(cwd)).await;
    let mut prompt = system_prompt(cwd);
    let block = crate::skills::skills_block(&catalog.winners);
    if !block.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
    (prompt, catalog)
}

pub(crate) struct ChatRuntime {
    pub(crate) transcript: RwLock<Vec<SessionMessageEntry>>,
    pub(crate) history: RwLock<Vec<AgentMessage>>,
    pub(crate) transcript_tx: watch::Sender<Arc<Vec<SessionMessageEntry>>>,
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
    /// The one-shot Title task's token (ADR-0012): independent of `cancel`
    /// — a Turn interrupt must not stop title generation; only chat
    /// deletion cancels it (in `AgentRuntime::remove_chat`).
    pub(crate) title_cancel: Mutex<Option<CancellationToken>>,
    /// Where this chat's transcript persists; empty for the ephemeral
    /// runtimes tests build directly.
    pub(crate) data_dir: PathBuf,
    pub(crate) chat_id: String,
    /// Set by `remove_chat`: the chat is deleted, so the settle pass of a
    /// run that was still alive (an open approval, a cancelled Turn) must
    /// write nothing back to disk — no resurrected transcript or History.
    pub(crate) removed: std::sync::atomic::AtomicBool,
    /// This chat's always-allow grants (ADR-0014): in-memory and
    /// session-scoped — a restart starts with none.
    pub(crate) grants: Mutex<crate::gate::GateGrants>,
}

/// Streaming publishes sample to this cadence (the doc-watch commit tick the
/// UI was tuned around): the watch itself coalesces via `send_replace`, so a
/// publish per delta token would only burn a full transcript snapshot per
/// token on the engine thread and starve the SSE pump.
const STREAM_PUBLISH_INTERVAL: Duration = Duration::from_millis(120);

impl ChatRuntime {
    #[cfg(test)]
    fn new() -> Self {
        let (transcript_tx, _) = watch::channel(Arc::new(Vec::new()));
        Self {
            transcript: RwLock::new(Vec::new()),
            history: RwLock::new(Vec::new()),
            transcript_tx,
            cancel: Mutex::new(None),
            title_cancel: Mutex::new(None),
            data_dir: PathBuf::new(),
            chat_id: String::new(),
            removed: std::sync::atomic::AtomicBool::new(false),
            grants: Mutex::new(crate::gate::GateGrants::default()),
        }
    }

    /// A chat whose transcript persists under `data_dir`, seeded from its
    /// last on-disk snapshot so reopening after a restart restores the
    /// conversation. A corrupt file starts empty rather than failing the
    /// open; the next publish overwrites it. The History (ADR-0010)
    /// replays the same way: the model-facing record is the replayed
    /// JSONL, so a reopened chat's next Turn carries what the Transcript
    /// shows. A missing record (a legacy chat) or a damaged one opens with
    /// an empty History and one persisted Transcript notice saying where
    /// the model's memory begins — written once, never rebuilt from the
    /// Transcript.
    fn load(data_dir: &Path, chat_id: &str, device_id: &str) -> Self {
        let mut transcript = load_transcript(data_dir, chat_id).unwrap_or_default();
        let replayed = crate::history::load_repaired(data_dir, chat_id);
        let (history, mut notice) = match replayed {
            Ok(repaired) => (repaired, None),
            Err(reason) => {
                // A damaged record never blocks the chat (ADR-0010): the
                // file is set aside — kept, never overwritten — and the
                // model starts over, with the reason on the notice.
                crate::history::quarantine(data_dir, chat_id);
                (
                    Vec::new(),
                    Some(format!(
                        "This chat's saved conversation could not be read ({reason}). \
                         The damaged file was set aside, and the model does not remember \
                         anything before this point."
                    )),
                )
            }
        };
        // A legacy chat — a Transcript on disk but no History file, from
        // before the record existed — opens with the same notice, worded
        // for pre-feature chats. The stable entry id is the written-once
        // guard (it also suppresses a legacy notice after a damaged one).
        if notice.is_none()
            && !transcript
                .iter()
                .any(|entry| entry.id == HISTORY_NOTICE_ENTRY_ID)
            && crate::store::transcript_path(data_dir, chat_id).is_some_and(|p| p.exists())
            && !crate::history::exists(data_dir, chat_id)
        {
            notice = Some(
                "This chat was kept from before holt saved the model's conversation. \
                 Everything above this line is visible to you, but the model starts \
                 fresh after it."
                    .into(),
            );
        }
        if let Some(message) = notice {
            transcript.push(SessionMessageEntry {
                id: HISTORY_NOTICE_ENTRY_ID.into(),
                role: MessageRole::System,
                parts: vec![MessagePart::Notice {
                    id: "n0".into(),
                    message,
                }],
                created_at: Utc::now().timestamp_millis(),
                device_id: device_id.to_string(),
                status: None,
                continuation_of: None,
            });
            // Written now, not on the next publish: the notice is part of
            // the record the moment the chat opens.
            let _ = persist_transcript(data_dir, chat_id, &transcript);
        }
        // An approval still pending on load belongs to a Turn the restart
        // ended (ADR-0014): settle it as aborted so the replayed chip never
        // poses as answerable.
        crate::gate::settle_pending_gates_on_load(&mut transcript);
        // The channel's initial value is the first frame subscribers see, so
        // seed it with the restored transcript: opening the watch replays it
        // as a whole-transcript `reset` without needing a publish.
        let (transcript_tx, _) = watch::channel(Arc::new(transcript.clone()));
        Self {
            transcript: RwLock::new(transcript),
            history: RwLock::new(history),
            transcript_tx,
            cancel: Mutex::new(None),
            title_cancel: Mutex::new(None),
            data_dir: data_dir.to_path_buf(),
            chat_id: chat_id.to_string(),
            removed: std::sync::atomic::AtomicBool::new(false),
            grants: Mutex::new(crate::gate::GateGrants::default()),
        }
    }

    pub(crate) fn publish(&self) {
        let transcript = self.transcript.read().unwrap_or_else(|e| e.into_inner());
        self.transcript_tx
            .send_replace(Arc::new(transcript.clone()));
        // Best-effort snapshot: the in-memory watch stays authoritative, and
        // a failed write surfaces again on the next publish instead of
        // failing the run that triggered it. A removed chat writes nothing —
        // a deleted transcript must not be resurrected by a settle pass.
        if !self.chat_id.is_empty() && !self.is_removed() {
            let _ = persist_transcript(&self.data_dir, &self.chat_id, &transcript);
        }
    }

    fn is_removed(&self) -> bool {
        self.removed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Append one completed message to the persisted History (ADR-0010):
    /// per-message as the Turn runs, never a whole-file rewrite at Turn
    /// end, so a crash mid-Turn loses nothing that had completed.
    /// Best-effort like the transcript snapshot — an unreadable tail is
    /// absorbed on load.
    pub(crate) fn append_history(&self, message: AgentMessage) {
        if self.chat_id.is_empty() || self.is_removed() {
            return;
        }
        if let Err(error) = crate::history::append_message(&self.data_dir, &self.chat_id, &message)
        {
            tracing::warn!(target: "holt::history", %error, "history append failed");
        }
    }
}

pub(crate) struct AgentRuntime {
    device_id: String,
    data_dir: PathBuf,
    pub(crate) chats: RwLock<Vec<Chat>>,
    pub(crate) chats_tx: watch::Sender<serde_json::Value>,
    sessions: RwLock<Vec<Session>>,
    pub(crate) sessions_tx: watch::Sender<serde_json::Value>,
    chat_runtime: Mutex<HashMap<String, Arc<ChatRuntime>>>,
    /// Open confirm-changes approvals across all chats (ADR-0014), keyed
    /// by approval id — the registry the `ResolveApproval` RPC addresses.
    /// Entries live only while their gate is open.
    pub(crate) approvals: Arc<crate::gate::ApprovalRegistry>,
    /// Test-injected provider transport (`EngineConfig::stream_fn`): every
    /// run's requests go through it instead of the built-in transport.
    /// Production leaves it unset.
    pub(crate) stream_fn: Option<pi_core::agent::types::StreamFn>,
}

impl AgentRuntime {
    pub(crate) fn new(
        device_id: String,
        data_dir: PathBuf,
        chats: Vec<Chat>,
        stream_fn: Option<pi_core::agent::types::StreamFn>,
    ) -> Self {
        let chats_value = serde_json::to_value(&chats).unwrap_or_else(|_| serde_json::json!([]));
        let (chats_tx, _) = watch::channel(chats_value);
        let (sessions_tx, _) = watch::channel(serde_json::json!([]));
        Self {
            device_id,
            data_dir,
            chats: RwLock::new(chats),
            chats_tx,
            sessions: RwLock::new(Vec::new()),
            sessions_tx,
            chat_runtime: Mutex::new(HashMap::new()),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            stream_fn,
        }
    }

    pub(crate) fn chat(&self, chat_id: &str) -> Arc<ChatRuntime> {
        let mut chats = self.chat_runtime.lock().unwrap_or_else(|e| e.into_inner());
        chats
            .entry(chat_id.to_string())
            .or_insert_with(|| {
                Arc::new(ChatRuntime::load(&self.data_dir, chat_id, &self.device_id))
            })
            .clone()
    }

    /// Drop a chat's runtime slot and its persisted records. An in-flight
    /// run is actively stopped (ADR-0014): a run paused behind an open
    /// approval would otherwise wait forever — and a late verdict could
    /// still execute a mutating tool for a chat the user deleted. The
    /// runtime is flagged removed first, so the aborted run's settle pass
    /// can neither resurrect the transcript file nor re-append History; a
    /// pending Title task is cancelled for the same reason (ADR-0012).
    pub(crate) fn remove_chat(&self, chat_id: &str) {
        let runtime = self
            .chat_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(chat_id);
        if let Some(runtime) = runtime {
            runtime
                .removed
                .store(true, std::sync::atomic::Ordering::Release);
            if let Some(token) = runtime
                .cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                token.cancel();
            }
            if let Some(token) = runtime
                .title_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                token.cancel();
            }
        }
        delete_transcript(&self.data_dir, chat_id);
        crate::history::delete_history(&self.data_dir, chat_id);
    }

    pub(crate) fn publish_chats(&self) {
        let chats = self.chats.read().unwrap_or_else(|e| e.into_inner());
        if let Ok(value) = serde_json::to_value(&*chats) {
            self.chats_tx.send_replace(value);
        }
    }

    /// Stamp the chat with the persisted "compact before next Turn" flag
    /// (the overflow fallback, ADR-0011): the last Turn ended on a context
    /// overflow, so the next Turn compacts unconditionally first.
    pub(crate) fn set_compact_before_next_turn(&self, chat_id: &str) {
        let mut chats = self.chats.write().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = chats.iter_mut().find(|row| row.id == chat_id) {
            row.compact_before_next_turn = true;
        }
        drop(chats);
        if crate::store::persist_chats(
            &self.data_dir,
            &self.chats.read().unwrap_or_else(|e| e.into_inner()),
        )
        .is_err()
        {
            tracing::warn!(target: "holt::agent", "could not persist the overflow flag");
        }
        self.publish_chats();
    }

    /// Read and clear the flag, persisting — consumed exactly once, by the
    /// Turn that acts on it.
    pub(crate) fn take_compact_before_next_turn(&self, chat_id: &str) -> bool {
        let mut chats = self.chats.write().unwrap_or_else(|e| e.into_inner());
        let flagged = chats
            .iter_mut()
            .find(|row| row.id == chat_id)
            .is_some_and(|row| {
                let flagged = row.compact_before_next_turn;
                row.compact_before_next_turn = false;
                flagged
            });
        drop(chats);
        if flagged {
            if crate::store::persist_chats(
                &self.data_dir,
                &self.chats.read().unwrap_or_else(|e| e.into_inner()),
            )
            .is_err()
            {
                tracing::warn!(target: "holt::agent", "could not persist clearing the overflow flag");
            }
            self.publish_chats();
        }
        flagged
    }

    pub(crate) fn set_session(&self, chat_id: &str, status: SessionStatus) {
        let now = Utc::now();
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        // A chat deleted mid-run (ADR-0014: an open approval's run is
        // actively cancelled) must not regain a session row from its own
        // settle pass — drop the row instead.
        let chat_exists = self
            .chats
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|row| row.id == chat_id);
        if !chat_exists {
            let before = sessions.len();
            sessions.retain(|session| session.chat_id != chat_id);
            if sessions.len() == before {
                return;
            }
        } else if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.chat_id == chat_id)
        {
            session.status = status;
            session.updated_at = now;
            if status == SessionStatus::Working && session.started_at.is_none() {
                session.started_at = Some(now);
            }
        } else {
            sessions.push(Session {
                chat_id: chat_id.to_string(),
                device_id: self.device_id.clone(),
                status,
                started_at: (status == SessionStatus::Working).then_some(now),
                updated_at: now,
            });
        }
        if let Ok(value) = serde_json::to_value(&*sessions) {
            self.sessions_tx.send_replace(value);
        }
    }
}

fn provider_reasoning(level: Option<ReasoningLevel>) -> Option<ProviderThinkingLevel> {
    level.map(|level| match level {
        ReasoningLevel::Minimal => ProviderThinkingLevel::Minimal,
        ReasoningLevel::Low => ProviderThinkingLevel::Low,
        ReasoningLevel::Medium => ProviderThinkingLevel::Medium,
        ReasoningLevel::High => ProviderThinkingLevel::High,
        ReasoningLevel::XHigh => ProviderThinkingLevel::Xhigh,
        ReasoningLevel::Max | ReasoningLevel::Ultra | ReasoningLevel::Ultracode => {
            ProviderThinkingLevel::Max
        }
        ReasoningLevel::Ultrathink => ProviderThinkingLevel::High,
    })
}

fn user_agent_message(text: String, timestamp: i64) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: RoleUser,
        content: UserContent::Text(text),
        timestamp,
    })
}

/// Decode a pi-core tool call into the transcript's decoded shape. Heavy
/// inputs (write content, edit strings) decode here and are stripped by the
/// render-only policy below; unknown tools keep their raw input so the chip
/// can still name them.
fn decode_tool_call(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> TranscriptToolCall {
    let arg = |key: &str| {
        arguments
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match name {
        "bash" => TranscriptToolCall::Exec {
            command: arg("command").unwrap_or_default(),
        },
        "read" => TranscriptToolCall::ReadFile {
            path: arg("path").unwrap_or_default(),
        },
        "write" => TranscriptToolCall::WriteFile {
            path: arg("path").unwrap_or_default(),
            content: None,
        },
        "edit" => TranscriptToolCall::EditFile {
            path: arg("path").unwrap_or_default(),
            old_string: None,
            new_string: None,
        },
        // The agent-facing `grep` API decodes into the pre-existing Search
        // chip; its other knobs
        // (glob, output_mode, …) are not carried — they show through the
        // tool output instead.
        "grep" => TranscriptToolCall::Search {
            pattern: arg("pattern").unwrap_or_default(),
            path: arg("path"),
        },
        other => TranscriptToolCall::Unknown {
            name: other.to_owned(),
            input: Some(serde_json::Value::Object(arguments.clone())),
        },
    }
}

fn transcript_tool_call(tool_call: &pi_core::ai::types::ToolCall) -> TranscriptToolCall {
    sanitize_tool_call(&decode_tool_call(&tool_call.name, &tool_call.arguments))
}

/// The full tool output persisted on the resolved tool part — a Read's file
/// content, the whole command transcript. pi-core bounds its builtins (read
/// truncates by lines/bytes), so results ride verbatim; the defensive ceiling
/// only keeps an unbounded MCP payload from flooding the doc.
fn tool_output_full(result: &AgentToolResult) -> Option<String> {
    const MAX_CHARS: usize = 1024 * 1024;
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            BlockContent::Text(text) => Some(text.text.as_str()),
            BlockContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.chars().count() <= MAX_CHARS {
        return (!text.trim().is_empty()).then_some(text);
    }
    let mut out: String = text.chars().take(MAX_CHARS).collect();
    out.push_str("\n…");
    Some(out)
}

/// Stamp a tool result onto the matching Tool part, wherever its entry sits.
fn resolve_tool_part(
    chat: &ChatRuntime,
    tool_call_id: &str,
    is_error: bool,
    output: Option<String>,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for entry in transcript.iter_mut() {
        let hit = entry
            .parts
            .iter_mut()
            .find(|part| matches!(part, MessagePart::Tool { id, .. } if id == tool_call_id));
        if let Some(MessagePart::Tool {
            resolved,
            is_error: part_error,
            output: part_output,
            ..
        }) = hit
        {
            *resolved = true;
            *part_error = is_error;
            *part_output = output.clone();
            changed = true;
        }
        if changed {
            break;
        }
    }
    drop(transcript);
    if changed {
        chat.publish();
    }
}

/// Append one housekeeping part (a compaction divider, a notice) as its
/// own System entry at the transcript's tail and publish — the record
/// grows, never shrinks (ADR-0011).
pub(crate) fn push_system_part(
    chat: &ChatRuntime,
    device_id: &str,
    entry_id: String,
    part: MessagePart,
) {
    chat.transcript
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(SessionMessageEntry {
            id: entry_id,
            role: MessageRole::System,
            parts: vec![part],
            created_at: Utc::now().timestamp_millis(),
            device_id: device_id.to_string(),
            status: None,
            continuation_of: None,
        });
    chat.publish();
}

/// The Transcript's row for one recorded compaction.
pub(crate) fn divider_part(record: &CompactionRecord) -> MessagePart {
    let CompactionRecord {
        summary,
        tokens_before,
        tokens_after,
        trigger,
        timestamp,
        ..
    } = record;
    MessagePart::CompactionDivider {
        id: "d0".into(),
        summary: summary.clone(),
        tokens_before: *tokens_before,
        tokens_after: *tokens_after,
        trigger: *trigger,
        timestamp: *timestamp,
    }
}

/// Record a Turn-boundary compaction: the `compaction` entry into the
/// History file and the divider as its own Transcript entry at the tail.
pub(crate) fn record_turn_start_compaction(
    chat: &ChatRuntime,
    device_id: &str,
    record: &CompactionRecord,
) {
    if let Err(error) = crate::history::append_compaction(&chat.data_dir, &chat.chat_id, record) {
        tracing::warn!(target: "holt::history", %error, "compaction entry append failed");
    }
    push_system_part(
        chat,
        device_id,
        format!("compaction-{}", uuid::Uuid::new_v4()),
        divider_part(record),
    );
}

/// Record a mid-Turn compaction (ADR-0011): the entry into the History
/// file — its position in the append-only record IS the ordering, it
/// summarizes exactly the messages before it — and the divider INTO the
/// run's live entry base, so it renders between the tool rows that
/// completed before it and whatever the Turn does next. The session
/// status does not change.
fn record_mid_turn_compaction(
    chat: &ChatRuntime,
    record: &CompactionRecord,
    run_base_parts: &Arc<Mutex<Vec<MessagePart>>>,
) {
    if let Err(error) = crate::history::append_compaction(&chat.data_dir, &chat.chat_id, record) {
        tracing::warn!(target: "holt::history", %error, "compaction entry append failed");
    }
    let MessagePart::CompactionDivider {
        summary,
        tokens_before,
        tokens_after,
        trigger,
        timestamp,
        ..
    } = divider_part(record)
    else {
        unreachable!("divider_part builds a divider");
    };
    let mut base = run_base_parts.lock().unwrap_or_else(|e| e.into_inner());
    let id = format!("d{timestamp}-{}", base.len());
    base.push(MessagePart::CompactionDivider {
        id,
        summary,
        tokens_before,
        tokens_after,
        trigger,
        timestamp,
    });
}

/// The built-in provider transport: the compat stream over the resolved
/// model. Tests inject their own through `EngineConfig::stream_fn`.
pub(crate) fn default_stream_fn() -> pi_core::agent::types::StreamFn {
    Arc::new(
        |model: &PiModel, context: &PiContext, options: Option<&SimpleStreamOptions>| {
            Ok(compat::stream_simple(model, context, options))
        },
    )
}

/// The base a run's loop continues from, shared with the mid-Turn
/// compaction hook and the end-of-run consolidation. `consumed` counts the
/// run's own messages a mid-Turn compaction folded into `history`, so the
/// consolidation never re-appends them.
struct RunBase {
    history: Vec<AgentMessage>,
    consumed: usize,
}

/// A run that ends early (abort, loop error) leaves tool parts without their
/// results; settle them so no chip stays "in call" forever.
fn settle_unresolved_tools(chat: &ChatRuntime) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed = false;
    for entry in transcript.iter_mut() {
        for part in entry.parts.iter_mut() {
            if let MessagePart::Tool { resolved, .. } = part
                && !*resolved
            {
                *resolved = true;
                changed = true;
            }
        }
    }
    drop(transcript);
    if changed {
        chat.publish();
    }
}

/// char length of a part for the run-cadence debug trace.
fn part_char_len(part: &MessagePart) -> usize {
    match part {
        MessagePart::Text { text, .. } | MessagePart::Reasoning { text, .. } => text.len(),
        MessagePart::Error { message, .. } => message.len(),
        _ => 0,
    }
}

/// The doc parts of one assistant message. `id_base` offsets the generated
/// text/thinking/error part ids: a run folds every message into ONE entry, so
/// per-message ids (`t0`, `r1`, …) must not collide across its messages — the
/// UI keys rows by `entry_id#part_id`.
///
/// `skill_files` maps this run's catalog `SKILL.md` paths (normalized
/// absolute) to skill names: a read of one collapses to the same skill chip
/// an invocation uses (ADR-0006) — the file's content reached the model
/// context through the tool result, and never enters the transcript.
fn assistant_parts(
    message: &AgentMessage,
    id_base: usize,
    cwd: &str,
    skill_files: &HashMap<String, String>,
) -> Vec<MessagePart> {
    let AgentMessage::Assistant(message) = message else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            AssistantContent::Text(text) => parts.push(MessagePart::Text {
                id: format!("t{}", id_base + parts.len()),
                text: text.text.clone(),
            }),
            AssistantContent::Thinking(thinking) if !thinking.thinking.is_empty() => {
                parts.push(MessagePart::Reasoning {
                    id: format!("r{}", id_base + parts.len()),
                    text: thinking.thinking.clone(),
                });
            }
            AssistantContent::ToolCall(tool_call) => {
                if let Some(part) = skill_read_part(tool_call, cwd, skill_files) {
                    parts.push(part);
                } else {
                    parts.push(MessagePart::Tool {
                        id: tool_call.id.clone(),
                        call: transcript_tool_call(tool_call),
                        is_error: false,
                        resolved: false,
                        output: None,
                        diff: None,
                        output_ref: None,
                        output_bytes: None,
                        diff_ref: None,
                        diff_stats: None,
                        subagent_ref: None,
                        subagent_status: None,
                        subagent_tail: None,
                        gate: None,
                    });
                }
            }
            AssistantContent::Thinking(_) => {}
        }
    }
    if let Some(error) = message.error_message.as_ref() {
        parts.push(MessagePart::Error {
            id: format!("e{}", id_base + parts.len()),
            message: error.clone(),
        });
    }
    parts
}

/// A read-tool call on a catalog skill's `SKILL.md`, collapsed to the skill
/// chip. Paths match after resolving the call's argument against the run's
/// cwd; any other file — including other `.md` files inside a skill's
/// directory — decodes as an ordinary read.
fn skill_read_part(
    tool_call: &pi_core::ai::types::ToolCall,
    cwd: &str,
    skill_files: &HashMap<String, String>,
) -> Option<MessagePart> {
    if tool_call.name != "read" {
        return None;
    }
    let path = tool_call.arguments.get("path")?.as_str()?;
    let resolved = crate::tools::to_absolute(cwd, path);
    let name = skill_files.get(&resolved)?;
    Some(MessagePart::Skill {
        id: tool_call.id.clone(),
        name: name.clone(),
        file: resolved,
        // The read result is not the chip's to carry (it lands in the
        // model context, not the doc); the file pointer stands in.
        content: None,
    })
}

/// Insert or refresh the run's live entry. `created_at` is stamped once at
/// first appearance — the delta protocol keys appends off an unchanged entry,
/// and the hover timestamp should say when the reply started anyway.
fn update_assistant_entry(
    chat: &ChatRuntime,
    entry_id: &str,
    parts: Vec<MessagePart>,
    status: MessageStatus,
    device_id: &str,
    publish: bool,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = transcript.iter_mut().find(|entry| entry.id == entry_id) {
        existing.parts = parts;
        existing.status = Some(status);
    } else {
        transcript.push(SessionMessageEntry {
            id: entry_id.to_string(),
            role: MessageRole::Assistant,
            parts,
            created_at: Utc::now().timestamp_millis(),
            device_id: device_id.to_string(),
            status: Some(status),
            continuation_of: None,
        });
    }
    drop(transcript);
    if publish {
        chat.publish();
    }
}

pub(crate) struct AgentRun {
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) chat_id: String,
    pub(crate) chat: Arc<ChatRuntime>,
    pub(crate) prompt: String,
    pub(crate) cwd: String,
    pub(crate) reasoning: Option<ReasoningLevel>,
    pub(crate) model: PiModel,
    pub(crate) api_key: String,
    pub(crate) timestamp: i64,
    pub(crate) cancel: CancellationToken,
    /// Root resolution for the run's skill listing (ADR-0005/0006).
    pub(crate) skills: crate::skills::Skills,
    /// A `/skill` invocation's chip (with the `<skill>` block the model
    /// received), seeded as the FIRST part of the run's entry — the agent
    /// reply opens with the invocation, ahead of any thinking.
    pub(crate) invocation: Option<MessagePart>,
    /// The Turn's permission-mode snapshot (ADR-0014), taken at acceptance:
    /// switches mid-Turn leave the running Turn under its original mode.
    pub(crate) permission_mode: PermissionMode,
    /// Test-injected provider transport; `None` means the built-in one.
    pub(crate) stream_fn: Option<pi_core::agent::types::StreamFn>,
}

pub(crate) async fn run_agent_command(run: AgentRun) {
    let AgentRun {
        runtime,
        chat_id,
        chat,
        prompt,
        cwd,
        reasoning,
        model,
        api_key,
        timestamp,
        cancel,
        skills,
        invocation,
        permission_mode,
        stream_fn,
    } = run;
    // The run's fresh skill catalog: one scan feeds the system-prompt block
    // AND the transcript's SKILL.md read collapsing — both see the same
    // live view of the roots.
    let (system_prompt, catalog) = run_system_prompt(&skills, &cwd).await;
    let skill_files: HashMap<String, String> = catalog
        .winners
        .iter()
        .map(|(skill, _)| (skill.file_path.clone(), skill.name.clone()))
        .collect();
    let entry_id = uuid::Uuid::new_v4().to_string();
    let sink_chat = chat.clone();
    let sink_device_id = runtime.device_id.clone();
    let sink_run_entry = entry_id.clone();
    // ONE transcript entry per run: every assistant message of the loop
    // appends its parts to the same entry (base holds the parts of the
    // messages that already ended — seeded with the invocation chip, so the
    // reply opens with the skill before any thinking), so a reply with N
    // tool round-trips renders as one message — one turn gap, one hover
    // timestamp/copy strip at its end. Per-message entries stamped a strip
    // mid-reply after every round-trip, which read as several half-finished
    // replies (user report), and tool results update parts by tool-call id
    // wherever they sit.
    let base_parts: Arc<Mutex<Vec<MessagePart>>> =
        Arc::new(Mutex::new(invocation.into_iter().collect()));
    let sink_base = base_parts.clone();
    let sink_last_publish = Arc::new(Mutex::new(None::<Instant>));
    let sink_run_start = Instant::now();
    let sink_cwd = cwd.clone();
    let sink_skill_files: Arc<HashMap<String, String>> = Arc::new(skill_files);
    let emit: AgentEventSink = Arc::new(move |event| {
        let chat = sink_chat.clone();
        let base_parts = sink_base.clone();
        let device_id = sink_device_id.clone();
        let run_entry = sink_run_entry.clone();
        let last_publish = sink_last_publish.clone();
        let cwd = sink_cwd.clone();
        let skill_files = sink_skill_files.clone();
        Box::pin(async move {
            // Debug trace of the event cadence: answers "did the reply
            // stream?" without a debugger — deltas arriving bunched here are
            // an upstream (provider/pi-core) shape, not a UI problem.
            let trace = |kind: &str, chars: usize, published: bool| {
                tracing::debug!(
                    target: "holt::agent",
                    at = ?sink_run_start.elapsed(),
                    kind,
                    chars,
                    published,
                    "run event"
                );
            };
            match event {
                AgentEvent::MessageStart { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len(), &cwd, &skill_files));
                    drop(base);
                    trace("start", parts.iter().map(part_char_len).sum(), true);
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        true,
                    );
                }
                AgentEvent::MessageUpdate { message, .. }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let due = {
                        let mut last = last_publish.lock().unwrap_or_else(|e| e.into_inner());
                        if last.is_none_or(|at| at.elapsed() >= STREAM_PUBLISH_INTERVAL) {
                            *last = Some(Instant::now());
                            true
                        } else {
                            false
                        }
                    };
                    let base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len(), &cwd, &skill_files));
                    drop(base);
                    trace("delta", parts.iter().map(part_char_len).sum(), due);
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        due,
                    );
                }
                AgentEvent::MessageEnd { message }
                    if matches!(&*message, AgentMessage::Assistant(_)) =>
                {
                    let mut base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
                    let mut parts = base.clone();
                    parts.extend(assistant_parts(&message, base.len(), &cwd, &skill_files));
                    *base = parts.clone();
                    drop(base);
                    // The next message's first delta must publish immediately.
                    *last_publish.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    // Status stays Streaming: the loop's settle pass stamps
                    // the terminal state once the WHOLE run returns, so the
                    // entry never poses as complete between tool rounds.
                    update_assistant_entry(
                        &chat,
                        &run_entry,
                        parts,
                        MessageStatus::Streaming,
                        &device_id,
                        true,
                    );
                    // The completed assistant message joins the persisted
                    // History as it ends (ADR-0010), in its History version:
                    // a message the run ends on is rewritten to a normal
                    // end (or dropped when it carries nothing but the
                    // error) — the repair invariant, applied per message.
                    if let AgentMessage::Assistant(assistant) = &*message
                        && let Some(for_history) = crate::history::history_assistant(assistant)
                    {
                        chat.append_history(AgentMessage::Assistant(Box::new(for_history)));
                    }
                }
                AgentEvent::MessageEnd { message }
                    if matches!(&*message, AgentMessage::ToolResult(_)) =>
                {
                    // Tool results land in the History as they complete —
                    // the loop emits one MessageEnd per tool result right
                    // after the tool finishes, so a crash mid-Turn keeps
                    // every completed call.
                    chat.append_history((*message).clone());
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    is_error,
                    ..
                } => {
                    resolve_tool_part(&chat, &tool_call_id, is_error, tool_output_full(&result));
                }
                _ => {}
            }
        })
    });

    let stream_fn = stream_fn.unwrap_or_else(default_stream_fn);
    let mut history = chat
        .history
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Compaction before the Turn's first request (ADR-0011): when the
    // History nears the model's context window, shrink it to a summary
    // plus a verbatim tail BEFORE the prompt is appended — the compaction
    // entry lands ahead of it in the file, so replay keeps the prompt
    // verbatim after the summary. The overflow fallback compacts the same
    // way UNCONDITIONALLY (the estimate already missed once), consuming
    // the persisted flag. Either way this is never a Turn: no
    // source-context stamping, no turn-diff baseline reset, no status
    // change.
    let overflow_recovery = runtime.take_compact_before_next_turn(&chat_id);
    let turn_start_compaction = if overflow_recovery {
        crate::compaction::compact_now(
            &history,
            &model,
            &stream_fn,
            &api_key,
            holt_doc::parts::CompactionTrigger::AfterOverflow,
            None,
        )
        .await
    } else {
        crate::compaction::compact(
            &history,
            &model,
            &stream_fn,
            &api_key,
            holt_doc::parts::CompactionTrigger::Automatic,
        )
        .await
    };
    match turn_start_compaction {
        Ok(Some(outcome)) => {
            record_turn_start_compaction(&chat, &runtime.device_id, &outcome.record);
            *chat.history.write().unwrap_or_else(|e| e.into_inner()) = outcome.messages.clone();
            history = outcome.messages;
        }
        Ok(None) => {}
        Err(reason) => {
            // A failed automatic compaction never blocks the Turn: proceed
            // uncompacted, with a visible notice (overflow, if it follows,
            // is the overflow fallback's business). An overflow recovery
            // that failed is still owed: keep the flag for the next Turn.
            if overflow_recovery {
                runtime.set_compact_before_next_turn(&chat_id);
            }
            tracing::warn!(target: "holt::compaction", %reason, "automatic compaction failed");
            push_system_part(
                &chat,
                &runtime.device_id,
                format!("compaction-failed-{}", uuid::Uuid::new_v4()),
                MessagePart::Notice {
                    id: "n0".into(),
                    message: format!(
                        "Automatic compaction failed ({reason}); the Turn continues \
                         with the full conversation."
                    ),
                },
            );
        }
    }
    // The base the loop continues from, shared with the mid-Turn
    // compaction hook: `history` is what the run started with, and
    // `consumed` counts the run's own messages a mid-Turn compaction
    // already folded into it (the end-of-run consolidation must not
    // re-append them).
    let run_base = Arc::new(Mutex::new(RunBase {
        history: history.clone(),
        consumed: 0,
    }));
    let prompt_message = user_agent_message(prompt, timestamp);
    // The user prompt joins the History when the Turn starts (ADR-0010) —
    // before any request, so even a Turn that dies immediately keeps what
    // the user asked.
    chat.append_history(prompt_message.clone());
    let mut stream_options = SimpleStreamOptions {
        reasoning: provider_reasoning(reasoning),
        ..Default::default()
    };
    stream_options.base.base.api_key = Some(api_key.clone());
    // Between tool rounds (ADR-0011): the same estimate-and-compact check
    // as the Turn-start one, through the loop's `prepare_next_turn` hook,
    // as often as the estimate calls for it — the round after a compaction
    // reports the compacted request's usage, which becomes the estimator's
    // anchor. A Turn that overflows anyway is the overflow fallback's
    // business. The session status never changes.
    let hook_chat = chat.clone();
    let hook_device_id = runtime.device_id.clone();
    let hook_model = model.clone();
    let hook_stream_fn = stream_fn.clone();
    let hook_base = Arc::clone(&run_base);
    let hook_base_parts = Arc::clone(&base_parts);
    let prepare_next_turn: pi_core::agent::types::PrepareNextTurnFn = Arc::new(
        move |last_turn: pi_core::agent::types::PrepareNextTurnContext| {
            let chat = hook_chat.clone();
            let model = hook_model.clone();
            let stream_fn = hook_stream_fn.clone();
            let api_key = api_key.clone();
            let base = Arc::clone(&hook_base);
            let base_parts = Arc::clone(&hook_base_parts);
            let device_id = hook_device_id.clone();
            Box::pin(async move {
                let pre_run_len = base.lock().unwrap_or_else(|e| e.into_inner()).history.len();
                if !crate::compaction::needed(&last_turn.context.messages, &model) {
                    return None;
                }
                let outcome = match crate::compaction::compact(
                    &last_turn.context.messages,
                    &model,
                    &stream_fn,
                    &api_key,
                    holt_doc::parts::CompactionTrigger::Automatic,
                )
                .await
                {
                    Ok(Some(outcome)) => outcome,
                    Ok(None) => return None,
                    Err(reason) => {
                        // Same rule as the Turn-start failure: the Turn
                        // continues uncompacted, visibly.
                        tracing::warn!(target: "holt::compaction", %reason, "mid-turn compaction failed");
                        push_system_part(
                            &chat,
                            &device_id,
                            format!("compaction-failed-{}", uuid::Uuid::new_v4()),
                            MessagePart::Notice {
                                id: "n0".into(),
                                message: format!(
                                    "Automatic compaction failed ({reason}); the Turn \
                                     continues with the full conversation."
                                ),
                            },
                        );
                        return None;
                    }
                };
                record_mid_turn_compaction(&chat, &outcome.record, &base_parts);
                // Run messages since the previous base — cumulative, since
                // the loop's returned list spans every compaction.
                let consumed = last_turn.context.messages.len().saturating_sub(pre_run_len);
                let mut context = last_turn.context.clone();
                context.messages = outcome.messages.clone();
                let mut guard = base.lock().unwrap_or_else(|e| e.into_inner());
                guard.history = outcome.messages;
                guard.consumed += consumed;
                drop(guard);
                Some(pi_core::agent::types::AgentLoopTurnUpdate {
                    context: Some(context),
                    model: None,
                    thinking_level: None,
                })
            })
        },
    );
    let overflow_model = model.clone();
    // The permission gate (ADR-0014): the before-tool-call hook that pauses
    // every mutating call behind the Turn's snapshotted mode. The title
    // task and compaction mount no tools and never pass through here.
    let gate = crate::gate::before_tool_call_hook(
        permission_mode,
        chat.clone(),
        Arc::clone(&base_parts),
        Arc::clone(&runtime.approvals),
        cwd.clone(),
        cancel.clone(),
    );
    let config = AgentLoopConfig {
        stream_options,
        model,
        // The harness converter (ADR-0011): identical to the pass-through
        // for ordinary messages, but renders the `compactionSummary`
        // custom message into the templated user message the model reads
        // after a compaction — the pass-through would drop it.
        convert_to_llm: Arc::new(|messages| {
            Box::pin(async move { harness_convert_to_llm(messages) })
        }),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: Some(prepare_next_turn),
        get_steering_messages: None,
        get_follow_up_messages: None,
        tool_execution: None,
        before_tool_call: Some(gate),
        after_tool_call: None,
    };
    let result = run_agent_loop(
        vec![prompt_message],
        AgentContext {
            system_prompt,
            messages: history,
            tools: Some(crate::tools::execution_tools(&cwd)),
        },
        config,
        emit,
        Some(cancel.clone()),
        Some(stream_fn),
    )
    .await;

    let errored = match result {
        Ok(messages) => {
            let errored = messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    AgentMessage::Assistant(message) => Some(message.error_message.is_some()),
                    _ => None,
                })
                .unwrap_or(false);
            let mut stored = chat.history.write().unwrap_or_else(|e| e.into_inner());
            // The base the loop ended on — the run's start history, or its
            // mid-Turn compacted replacement — plus the run's messages a
            // mid-Turn compaction had NOT already folded into it, in their
            // repaired form. This is exactly what the file replays: the
            // compaction entry summarizes everything before it and keeps
            // the tail, and only the post-compaction messages follow.
            let base = run_base.lock().unwrap_or_else(|e| e.into_inner());
            *stored = base.history.clone();
            let remaining = &messages[base.consumed.min(messages.len())..];
            let repaired = crate::history::repair_history(remaining);
            for message in &repaired {
                stored.push(message.clone());
            }
            drop(stored);
            // Disk already carries every completed message from the sink;
            // the sweep only adds the synthetic results for calls the run
            // ended before they could execute.
            for message in crate::history::interrupted_results_for(&messages) {
                chat.append_history(message);
            }
            // An interrupted loop never sends the closing MessageEnd, so its
            // last entry would stream forever — settle it here.
            let end_status = if cancel.is_cancelled() || errored {
                MessageStatus::Aborted
            } else {
                MessageStatus::Complete
            };
            let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
            let mut settled = false;
            for entry in transcript.iter_mut() {
                if entry.status == Some(MessageStatus::Streaming) {
                    entry.status = Some(end_status);
                    settled = true;
                }
            }
            drop(transcript);
            if settled {
                chat.publish();
            }
            // The overflow fallback (ADR-0011): the estimator missed and
            // the provider said so (or the usage silently exceeded the
            // window). Stamp the chat — the NEXT Turn compacts
            // unconditionally — and say so readably. No in-Turn retry.
            let overflowed = messages.iter().rev().find_map(|message| match message {
                AgentMessage::Assistant(assistant) => {
                    Some(pi_core::ai::utils::overflow::is_context_overflow(
                        assistant,
                        Some(overflow_model.context_window),
                    ))
                }
                _ => None,
            });
            if overflowed == Some(true) {
                runtime.set_compact_before_next_turn(&chat_id);
                push_system_part(
                    &chat,
                    &runtime.device_id,
                    format!("overflow-{}", uuid::Uuid::new_v4()),
                    MessagePart::Notice {
                        id: "n0".into(),
                        message: "This conversation outgrew the model's context window. \
                                  The next message will first compact the conversation \
                                  into a summary, then continue."
                            .into(),
                    },
                );
            }
            errored
        }
        Err(error) => {
            let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = transcript.iter_mut().find(|e| e.id == entry_id) {
                // The loop died mid-reply: surface the error on the live entry
                // and settle it, rather than pushing a second entry that
                // reuses its id.
                existing.parts.push(MessagePart::Error {
                    id: format!("e{}", existing.parts.len()),
                    message: error,
                });
                existing.status = Some(MessageStatus::Aborted);
            } else {
                // The loop died before its first message: the entry never
                // materialized, so build it here — the invocation seed
                // (if any) still leads, the error closes.
                let mut parts = base_parts.lock().unwrap_or_else(|e| e.into_inner()).clone();
                parts.push(MessagePart::Error {
                    id: format!("e{}", parts.len()),
                    message: error,
                });
                transcript.push(SessionMessageEntry {
                    id: entry_id,
                    role: MessageRole::Assistant,
                    parts,
                    created_at: Utc::now().timestamp_millis(),
                    device_id: runtime.device_id.clone(),
                    status: Some(MessageStatus::Complete),
                    continuation_of: None,
                });
            }
            drop(transcript);
            chat.publish();
            true
        }
    };
    settle_unresolved_tools(&chat);
    *chat.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    runtime.set_session(
        &chat_id,
        if errored {
            SessionStatus::Errored
        } else {
            SessionStatus::Idle
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{AssistantMessage, TextContent, ThinkingContent, ToolCall};

    fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            content_type: Default::default(),
            id: "call-1".into(),
            name: name.into(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            namespace: None,
        }
    }

    #[test]
    fn assistant_message_maps_text_and_reasoning_to_doc_parts() {
        let message = AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "plan".into(),
                    ..Default::default()
                }),
                AssistantContent::Text(TextContent {
                    text: "answer".into(),
                    ..Default::default()
                }),
            ],
            ..Default::default()
        }));
        assert_eq!(
            assistant_parts(&message, 0, "/tmp/x", &HashMap::new()),
            vec![
                MessagePart::Reasoning {
                    id: "r0".into(),
                    text: "plan".into(),
                },
                MessagePart::Text {
                    id: "t1".into(),
                    text: "answer".into(),
                },
            ]
        );
        // A second message of the same run folds into the same entry: its
        // generated part ids continue after the base so row keys never
        // collide.
        assert_eq!(
            assistant_parts(&message, 2, "/tmp/x", &HashMap::new()),
            vec![
                MessagePart::Reasoning {
                    id: "r2".into(),
                    text: "plan".into(),
                },
                MessagePart::Text {
                    id: "t3".into(),
                    text: "answer".into(),
                },
            ]
        );
    }

    #[test]
    fn system_prompt_includes_working_directory() {
        let prompt = system_prompt("/tmp/holt");
        assert!(prompt.contains("/tmp/holt"));
        assert!(!prompt.contains("{{cwd}}"));
    }

    /// Write a skill into a temp personal root the way the loader expects.
    fn write_skill(root: &std::path::Path, name: &str, frontmatter: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\nbody\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn run_system_prompt_appends_a_fresh_metadata_only_skills_block() {
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        std::fs::create_dir_all(&personal).unwrap();
        let cwd = base.path().join("cwd");
        let skills = crate::skills::Skills::new(&base.path().join("data"), Some(&personal));

        // No skills: the template stands alone.
        let (bare, bare_catalog) = run_system_prompt(&skills, &cwd.to_string_lossy()).await;
        assert_eq!(bare, system_prompt(&cwd.to_string_lossy()));
        assert!(bare_catalog.winners.is_empty());

        write_skill(
            &personal,
            "grill",
            "name: grill\ndescription: Grill a plan.\n",
        );
        write_skill(
            &personal,
            "hidden",
            "name: hidden\ndescription: Manual only.\ndisable-model-invocation: true\n",
        );
        let (prompt, catalog) = run_system_prompt(&skills, &cwd.to_string_lossy()).await;
        assert!(prompt.starts_with(&system_prompt(&cwd.to_string_lossy())));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>grill</name>"));
        // The catalog the run would hand its transcript decoder: every
        // winner's file path maps to its name.
        let skill_files: HashMap<String, String> = catalog
            .winners
            .iter()
            .map(|(skill, _)| (skill.file_path.clone(), skill.name.clone()))
            .collect();
        assert!(skill_files.keys().all(|path| path.ends_with("/grill/SKILL.md")
            || path.ends_with("/hidden/SKILL.md")));
        // disable-model-invocation stays out of the advertisement but is
        // still cataloged — and content never appears at all.
        assert!(!prompt.contains("hidden"));
        assert!(!prompt.contains("body"));

        // A pathological catalog cannot crowd the task out of the window:
        // the appended block stays under its budget however much is on disk.
        let long = "very long description ".repeat(120);
        for i in 0..400 {
            write_skill(
                &personal,
                &format!("bulk-{i:03}"),
                &format!("name: bulk-{i:03}\ndescription: {long}\n"),
            );
        }
        let (capped, _) = run_system_prompt(&skills, &cwd.to_string_lossy()).await;
        let template_len = system_prompt(&cwd.to_string_lossy()).chars().count();
        assert!(capped.chars().count() <= template_len + 2 + crate::skills::SKILL_LISTING_BUDGET);
    }

    #[test]
    fn assistant_message_maps_tool_calls_to_tool_parts() {
        let message = AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![AssistantContent::ToolCall(tool_call(
                "bash",
                serde_json::json!({ "command": "ls -la" }),
            ))],
            ..Default::default()
        }));
        assert_eq!(
            assistant_parts(&message, 0, "/tmp/x", &HashMap::new()),
            vec![MessagePart::Tool {
                id: "call-1".into(),
                call: TranscriptToolCall::Exec {
                    command: "ls -la".into(),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                gate: None,
            }]
        );
    }

    #[test]
    fn tool_calls_decode_to_known_shapes_and_strip_heavy_inputs() {
        assert_eq!(
            transcript_tool_call(&tool_call("read", serde_json::json!({ "path": "a.rs" }))),
            TranscriptToolCall::ReadFile {
                path: "a.rs".into()
            }
        );
        // Write content is stripped before it can reach the doc.
        assert_eq!(
            transcript_tool_call(&tool_call(
                "write",
                serde_json::json!({ "path": "a.rs", "content": "lots of text" })
            )),
            TranscriptToolCall::WriteFile {
                path: "a.rs".into(),
                content: None,
            }
        );
        assert_eq!(
            transcript_tool_call(&tool_call(
                "edit",
                serde_json::json!({ "path": "a.rs", "edits": [{ "oldText": "x", "newText": "y" }] })
            )),
            TranscriptToolCall::EditFile {
                path: "a.rs".into(),
                old_string: None,
                new_string: None,
            }
        );
        // Unknown tools degrade to a named chip, input intact (the policy
        // strips non-spawn inputs).
        let decoded = transcript_tool_call(&tool_call(
            "web_search",
            serde_json::json!({ "query": "holt" }),
        ));
        assert!(matches!(
            decoded,
            TranscriptToolCall::Unknown { ref name, input: None } if name == "web_search"
        ));
    }

    #[test]
    fn catalog_skill_md_reads_collapse_to_the_skill_chip() {
        let skill_files: HashMap<String, String> =
            HashMap::from([("/roots/grill/SKILL.md".to_string(), "grill".to_string())]);
        let read = |path: &str| {
            let message = AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![AssistantContent::ToolCall(tool_call(
                    "read",
                    serde_json::json!({ "path": path }),
                ))],
                ..Default::default()
            }));
            assistant_parts(&message, 0, "/roots", &skill_files)
        };
        // The advertised location, absolute…
        assert_eq!(
            read("/roots/grill/SKILL.md"),
            vec![MessagePart::Skill {
                id: "call-1".into(),
                name: "grill".into(),
                file: "/roots/grill/SKILL.md".into(),
                content: None,
            }]
        );
        // …and the same file reached through a relative path.
        assert_eq!(
            read("grill/SKILL.md"),
            vec![MessagePart::Skill {
                id: "call-1".into(),
                name: "grill".into(),
                file: "/roots/grill/SKILL.md".into(),
                content: None,
            }]
        );
        // Any other file — including another `.md` inside the skill's own
        // directory — renders as an ordinary read.
        assert_eq!(
            read("/roots/grill/notes.md"),
            vec![MessagePart::Tool {
                id: "call-1".into(),
                call: TranscriptToolCall::ReadFile {
                    path: "/roots/grill/notes.md".into(),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                gate: None,
            }]
        );
        // An empty catalog (the file stopped being a skill) renders the
        // plain read again — detection keys off the live catalog.
        let message = AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![AssistantContent::ToolCall(tool_call(
                "read",
                serde_json::json!({ "path": "/roots/grill/SKILL.md" }),
            ))],
            ..Default::default()
        }));
        let parts = assistant_parts(&message, 0, "/roots", &HashMap::new());
        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                call: TranscriptToolCall::ReadFile { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn collapse_keys_agree_with_the_real_loader_paths() {
        // The map the run builds and the read-argument resolver must agree
        // on the loader's own file paths — hand-built maps can't catch a
        // normalization divergence, a real scan can.
        let base = tempfile::tempdir().unwrap();
        let personal = base.path().join("personal");
        std::fs::create_dir_all(&personal).unwrap();
        write_skill(
            &personal,
            "grill",
            "name: grill\ndescription: Grill a plan.\n",
        );
        let skills = crate::skills::Skills::new(&base.path().join("data"), Some(&personal));
        let catalog = skills.catalog(None).await;
        let skill_files: HashMap<String, String> = catalog
            .winners
            .iter()
            .map(|(skill, _)| (skill.file_path.clone(), skill.name.clone()))
            .collect();
        let skill_file = personal.join("grill").join("SKILL.md");
        let skill_file = skill_file.to_string_lossy().into_owned();
        assert_eq!(
            skill_read_part(
                &tool_call("read", serde_json::json!({ "path": skill_file })),
                "/",
                &skill_files,
            ),
            Some(MessagePart::Skill {
                id: "call-1".into(),
                name: "grill".into(),
                file: skill_files
                    .keys()
                    .find(|path| path.ends_with("grill/SKILL.md"))
                    .cloned()
                    .unwrap(),
                content: None,
            })
        );
    }

    #[test]
    fn transcript_survives_runtime_restart() {
        let dir = std::env::temp_dir().join(format!("holt-restart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = AgentRuntime::new("device".into(), dir.clone(), Vec::new(), None);
        let chat = runtime.chat("chat-1");
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into(),
            }],
            created_at: 1,
            device_id: "device".into(),
            status: None,
            continuation_of: None,
        });
        chat.publish();

        // A fresh runtime over the same data dir replays the persisted
        // transcript both in memory and as the watch's opening `reset` frame.
        // With no History file beside it, this is a legacy chat: the replay
        // ends with the one written-once notice marking where the model's
        // memory begins.
        let restarted = AgentRuntime::new("device".into(), dir.clone(), Vec::new(), None);
        let restored = restarted.chat("chat-1");
        let transcript = restored.transcript.read().unwrap();
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].id, "m1");
        assert_eq!(transcript[1].id, HISTORY_NOTICE_ENTRY_ID);
        assert!(matches!(
            transcript[1].parts.first(),
            Some(MessagePart::Notice { .. })
        ));
        drop(transcript);
        assert_eq!(restored.transcript_tx.borrow().len(), 2);
        // Reopening never adds a second notice.
        let again = AgentRuntime::new("device".into(), dir.clone(), Vec::new(), None);
        assert_eq!(again.chat("chat-1").transcript.read().unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_tool_part_stamps_the_matching_chip() {
        let chat = ChatRuntime::new();
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: "entry-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: "call-9".into(),
                call: TranscriptToolCall::Exec {
                    command: "sleep 1".into(),
                },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                gate: None,
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        });
        resolve_tool_part(&chat, "call-9", true, Some("boom".into()));
        let transcript = chat.transcript.read().unwrap();
        let Some(MessagePart::Tool {
            resolved,
            is_error,
            output,
            ..
        }) = transcript[0].parts.first()
        else {
            panic!("expected a tool part");
        };
        assert!(*resolved);
        assert!(*is_error);
        assert_eq!(output.as_deref(), Some("boom"));
    }

    #[test]
    fn run_messages_fold_into_one_entry_with_stable_created_at() {
        let chat = ChatRuntime::new();
        let text = |text: &str| {
            AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: text.into(),
                    ..Default::default()
                })],
                ..Default::default()
            }))
        };
        // First message creates the entry, later ones replace its parts in
        // place — same id, same created_at (the delta protocol keys appends
        // off an unchanged entry and the strip stamps once).
        let mut first_parts = assistant_parts(&text("hello"), 0, "/tmp/x", &HashMap::new());
        update_assistant_entry(
            &chat,
            "run-1",
            first_parts.clone(),
            MessageStatus::Streaming,
            "device",
            true,
        );
        let created_at = chat.transcript.read().unwrap()[0].created_at;
        let second = assistant_parts(
            &text(" world"),
            first_parts.len(),
            "/tmp/x",
            &HashMap::new(),
        );
        first_parts.extend(second);
        update_assistant_entry(
            &chat,
            "run-1",
            first_parts,
            MessageStatus::Streaming,
            "device",
            false,
        );
        let transcript = chat.transcript.read().unwrap();
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].created_at, created_at);
        assert_eq!(
            transcript[0].parts,
            vec![
                MessagePart::Text {
                    id: "t0".into(),
                    text: "hello".into(),
                },
                MessagePart::Text {
                    id: "t1".into(),
                    text: " world".into(),
                },
            ]
        );
    }
}
