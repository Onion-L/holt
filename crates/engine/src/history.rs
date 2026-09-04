//! The History record (ADR-0010): a chat's model-facing message sequence
//! persisted as its own append-only JSONL file next to the Transcript — a
//! version header line, then one entry per line. The only entry kind today
//! is `message` (an [`AgentMessage`] in its upstream serde shape,
//! unchanged); the file is holt-owned, so later kinds can be added without
//! binding to upstream session codecs. Reads are tolerant: unknown entry
//! kinds are skipped (unknown message roles already route to the upstream
//! `Custom` variant) and a truncated or unparsable trailing line — the
//! crash-mid-append shape — is treated as absent.

use std::io::Write;
use std::path::{Path, PathBuf};

use pi_core::agent::types::AgentMessage;
use pi_core::ai::types::{
    AssistantContent, AssistantMessage, BlockContent, StopReason, TextContent, ToolResultMessage,
};

use crate::store::chat_id_is_path_safe;

/// The History format version carried by the header line.
const HISTORY_VERSION: u32 = 1;

/// Per-chat History file, guarded by the same chat-id path-safety rule as
/// the Transcript (see [`crate::store::transcript_path`]).
pub(crate) fn history_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !chat_id_is_path_safe(chat_id) {
        return None;
    }
    Some(data_dir.join("history").join(format!("{chat_id}.jsonl")))
}

/// One JSONL entry: `{"kind":"message","entry":{…AgentMessage…}}` or
/// `{"kind":"compaction","entry":{…CompactionRecord…}}`. The adjacent-tag
/// shape keeps the entry self-describing for the tolerant reader below.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "entry", rename_all = "camelCase")]
pub(crate) enum HistoryEntry {
    Message(AgentMessage),
    Compaction(CompactionRecord),
}

/// A Compaction of the History (ADR-0011): everything before this entry is
/// replaced on replay by the summary, with the last `retained_tail`
/// messages kept verbatim (they are the file's own preceding message
/// entries — the count is the replay rule, the messages are not
/// duplicated into the record).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionRecord {
    pub summary: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub trigger: holt_doc::parts::CompactionTrigger,
    pub timestamp: i64,
    pub retained_tail: u64,
}

/// Append one entry, creating the file (header first) when the chat has no
/// History yet. Append-only by construction: a Turn's messages land as
/// they complete, never as a whole-file rewrite.
pub(crate) fn append_entry(
    data_dir: &Path,
    chat_id: &str,
    entry: &HistoryEntry,
) -> std::io::Result<()> {
    let Some(path) = history_path(data_dir, chat_id) else {
        return Ok(());
    };
    let dir = path
        .parent()
        .expect("history path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let line =
        serde_json::to_string(entry).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // Header on an empty file, not on a new one: a crash between create
    // and the header write leaves a zero-byte file that must not collect
    // headerless entries.
    if file.metadata()?.len() == 0 {
        let header = serde_json::json!({ "version": HISTORY_VERSION });
        writeln!(file, "{header}")?;
    }
    writeln!(file, "{line}")
}

/// Append one completed message to the chat's History.
pub(crate) fn append_message(
    data_dir: &Path,
    chat_id: &str,
    message: &AgentMessage,
) -> std::io::Result<()> {
    append_entry(data_dir, chat_id, &HistoryEntry::Message(message.clone()))
}

/// Append a compaction record to the chat's History. Must be written only
/// after the compacted history is in effect — replay discards everything
/// before it.
pub(crate) fn append_compaction(
    data_dir: &Path,
    chat_id: &str,
    record: &CompactionRecord,
) -> std::io::Result<()> {
    append_entry(data_dir, chat_id, &HistoryEntry::Compaction(record.clone()))
}

/// Replay the History linearly into the in-memory message sequence. A
/// missing file is an empty History (a chat that never ran — or a legacy
/// chat, whose notice arrives with its slice); an unreadable header or an
/// unknown version is an error the caller decides how to surface.
pub(crate) fn load(data_dir: &Path, chat_id: &str) -> Result<Vec<AgentMessage>, String> {
    let Some(path) = history_path(data_dir, chat_id) else {
        return Ok(Vec::new());
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // A chat that never ran — or whose id fails the path-safety rule —
        // has no History: one "no History" shape for callers.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("{}: history file has no header line", path.display()))?;
    let version = serde_json::from_str::<serde_json::Value>(header)
        .ok()
        .and_then(|value| value.get("version").and_then(|v| v.as_u64()))
        .ok_or_else(|| format!("{}: unreadable history header", path.display()))?;
    if version != HISTORY_VERSION as u64 {
        return Err(format!(
            "{}: unknown history format version {version} (supported: {HISTORY_VERSION})",
            path.display()
        ));
    }
    let mut messages = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A truncated or unparsable trailing line is the crash-mid-
            // append shape: treat it as absent rather than failing the
            // whole record.
            tracing::warn!(target: "holt::history", "skipping unparsable history line");
            continue;
        };
        match value.get("kind").and_then(|kind| kind.as_str()) {
            Some("message") => match serde_json::from_value::<AgentMessage>(
                value
                    .get("entry")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            ) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    tracing::warn!(target: "holt::history", %error, "skipping undecodable history entry")
                }
            },
            Some("compaction") => {
                match serde_json::from_value::<CompactionRecord>(
                    value
                        .get("entry")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ) {
                    Ok(record) => {
                        // Linear replay (ADR-0011): the compaction replaces
                        // everything before it with the upstream
                        // `compactionSummary` custom message followed by the
                        // retained tail — the file's own preceding message
                        // entries, recovered by count.
                        let tail_count = record.retained_tail as usize;
                        let tail = if messages.len() >= tail_count {
                            messages.split_off(messages.len() - tail_count)
                        } else {
                            std::mem::take(&mut messages)
                        };
                        messages.push(
                            pi_core::agent::harness::messages::create_compaction_summary_message(
                                record.summary,
                                record.tokens_before,
                                record.timestamp,
                            ),
                        );
                        messages.extend(tail);
                    }
                    Err(error) => {
                        tracing::warn!(target: "holt::history", %error, "skipping undecodable compaction entry")
                    }
                }
            }
            other => {
                // Unknown entry kinds are tolerated: a future holt wrote
                // them, an older model must not choke on them.
                tracing::warn!(target: "holt::history", ?other, "skipping unknown history entry kind");
            }
        }
    }
    Ok(messages)
}

/// Whether the chat has a History file on disk (a missing one alongside an
/// existing Transcript means a legacy chat — see `ChatRuntime::load`).
pub(crate) fn exists(data_dir: &Path, chat_id: &str) -> bool {
    history_path(data_dir, chat_id).is_some_and(|path| path.exists())
}

/// Rename a damaged History file aside with a timestamped `.corrupt`
/// suffix — kept, never overwritten or deleted, so nothing is silently
/// thrown away even when the same chat is damaged twice. The chat opens
/// with an empty History; the next Turn starts a fresh file.
pub(crate) fn quarantine(data_dir: &Path, chat_id: &str) {
    let Some(path) = history_path(data_dir, chat_id) else {
        return;
    };
    let stamp = chrono::Utc::now().timestamp_millis();
    let mut aside = path.with_extension(format!("jsonl.{stamp}.corrupt"));
    let mut bump = 1;
    while aside.exists() {
        aside = path.with_extension(format!("jsonl.{stamp}-{bump}.corrupt"));
        bump += 1;
    }
    if let Err(error) = std::fs::rename(&path, &aside)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(target: "holt::history", %error, "could not set the damaged history aside");
    }
}

/// Drop a chat's persisted History. Missing files are fine — chats that
/// never ran have nothing on disk.
pub(crate) fn delete_history(data_dir: &Path, chat_id: &str) {
    if let Some(path) = history_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// The repair invariant (ADR-0010): the History must always be a valid
// provider request payload — every tool call has a matching tool result,
// and the record never ends on a half-streamed assistant message.
// ---------------------------------------------------------------------------

/// The History's version of one finished assistant message. A message the
/// run ended on — aborted by the user, or carrying a provider error — is
/// rewritten to a normal end: the model learns what happened from the
/// synthetic interrupted tool results, not from a stop reason that makes
/// the next request invalid (upstream's request-time normalizer would drop
/// an aborted message outright, hiding the interruption from the model).
/// A terminal message with no content at all is dropped — it carries
/// nothing, providers reject empty assistant turns, and (for the
/// interrupted-mid-batch shape, where the loop emits one after the
/// aborted tool results) it would sit between a call and its synthetic
/// result. The Transcript keeps the visible error either way.
pub(crate) fn history_assistant(assistant: &AssistantMessage) -> Option<AssistantMessage> {
    match assistant.stop_reason {
        StopReason::Error | StopReason::Aborted if assistant.content.is_empty() => None,
        StopReason::Error | StopReason::Aborted => {
            let mut repaired = assistant.clone();
            repaired.stop_reason = StopReason::Stop;
            repaired.error_message = None;
            Some(repaired)
        }
        _ => Some(assistant.clone()),
    }
}

/// Tool calls in `messages` that never received their result, in call
/// order — the crash-mid-run and interrupted-mid-run shapes.
fn dangling_tool_calls(messages: &[AgentMessage]) -> Vec<(String, String)> {
    let mut dangling = Vec::new();
    for message in messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(call) = block {
                        dangling.push((call.id.clone(), call.name.clone()));
                    }
                }
            }
            AgentMessage::ToolResult(result) => {
                dangling.retain(|(id, _)| *id != result.tool_call_id);
            }
            _ => {}
        }
    }
    dangling
}

/// A synthetic error tool result for a call that never ran: an honest
/// "this was interrupted" record, distinguishable from a real failure.
/// holt writes these itself so upstream's request-time normalizer ("No
/// result provided") never has anything to synthesize.
fn interrupted_tool_result(tool_call_id: &str, tool_name: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: Default::default(),
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![BlockContent::Text(TextContent {
            text: format!(
                "The Turn was interrupted by the user; the \"{tool_name}\" tool call was not executed."
            ),
            ..Default::default()
        })],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: true,
        timestamp: 0,
    }))
}

/// The synthetic interrupted-results for calls in `messages` that never
/// got their result — the run-end sweep's disk appends (everything else
/// already landed per-message as it completed).
pub(crate) fn interrupted_results_for(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    dangling_tool_calls(messages)
        .into_iter()
        .map(|(id, name)| interrupted_tool_result(&id, &name))
        .collect()
}

/// Enforce the invariant over a whole History sequence: rewrite or drop
/// terminal assistant messages, and give every dangling tool call its
/// synthetic interrupted result — placed with the results of the message
/// that made the call, where providers require it, not at the tail. Runs
/// after replay on load — the crash-truncated tail never reaches the
/// model as-is.
pub(crate) fn repair_history(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut repaired = Vec::with_capacity(messages.len());
    // Calls of the most recent assistant message still awaiting results.
    let mut open: Vec<(String, String)> = Vec::new();
    let flush = |open: &mut Vec<(String, String)>, repaired: &mut Vec<AgentMessage>| {
        for (id, name) in open.drain(..) {
            repaired.push(interrupted_tool_result(&id, &name));
        }
    };
    for message in messages {
        match message {
            AgentMessage::ToolResult(result) => {
                open.retain(|(id, _)| *id != result.tool_call_id);
                repaired.push(message.clone());
            }
            AgentMessage::Assistant(assistant) => {
                flush(&mut open, &mut repaired);
                let Some(repaired_message) = history_assistant(assistant) else {
                    continue;
                };
                open = repaired_message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => {
                            Some((call.id.clone(), call.name.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                repaired.push(AgentMessage::Assistant(Box::new(repaired_message)));
            }
            other => {
                flush(&mut open, &mut repaired);
                repaired.push(other.clone());
            }
        }
    }
    flush(&mut open, &mut repaired);
    repaired
}

/// Replay and repair a chat's History, persisting the synthetic results
/// the repair added so the file and the in-memory sequence agree on the
/// next load (a load-time repair that stayed in memory would be re-derived
/// after later Turns, at the tail, away from the call it answers). The
/// record stays append-only: the additions go at the end of the file,
/// which is where a crash-truncated tail's dangling calls are.
pub(crate) fn load_repaired(data_dir: &Path, chat_id: &str) -> Result<Vec<AgentMessage>, String> {
    let replayed = load(data_dir, chat_id)?;
    let repaired = repair_history(&replayed);
    let answered: Vec<&str> = replayed
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    for message in &repaired {
        let AgentMessage::ToolResult(result) = message else {
            continue;
        };
        if answered.contains(&result.tool_call_id.as_str()) {
            continue;
        }
        if let Err(error) = append_message(data_dir, chat_id, message) {
            tracing::warn!(target: "holt::history", %error, "could not persist a load-time repair");
        }
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{AssistantMessage, RoleUser, UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            role: RoleUser,
            content: UserContent::Text(text.into()),
            timestamp: 1,
        })
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![pi_core::ai::types::AssistantContent::Text(
                pi_core::ai::types::TextContent {
                    text: text.into(),
                    ..Default::default()
                },
            )],
            model: "mock".into(),
            ..Default::default()
        }))
    }

    /// An assistant message that ended a run the hard way — `stop` is the
    /// terminal stop reason, `content` whatever streamed before the end.
    fn terminal_assistant(stop: StopReason, content: Vec<AssistantContent>) -> AgentMessage {
        AgentMessage::Assistant(Box::new(AssistantMessage {
            content,
            stop_reason: stop,
            error_message: (stop != StopReason::Stop).then(|| "the failure".into()),
            model: "mock".into(),
            ..Default::default()
        }))
    }

    fn tool_call_block(id: &str, name: &str) -> AssistantContent {
        AssistantContent::ToolCall(pi_core::ai::types::ToolCall {
            id: id.into(),
            name: name.into(),
            ..Default::default()
        })
    }

    fn text_block(text: &str) -> AssistantContent {
        AssistantContent::Text(TextContent {
            text: text.into(),
            ..Default::default()
        })
    }

    fn tool_result(id: &str) -> AgentMessage {
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![BlockContent::Text(TextContent {
                text: "ran".into(),
                ..Default::default()
            })],
            ..Default::default()
        }))
    }

    fn result_ids(messages: &[AgentMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::ToolResult(result) => Some(result.tool_call_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn result_text(result: &ToolResultMessage) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                BlockContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn repair_gives_each_dangling_call_one_interrupted_result() {
        let messages = vec![
            user("tidy up"),
            terminal_assistant(
                StopReason::Aborted,
                vec![
                    tool_call_block("call-1", "bash"),
                    tool_call_block("call-2", "read"),
                    tool_call_block("call-3", "edit"),
                ],
            ),
        ];
        let repaired = repair_history(&messages);
        assert_eq!(repaired.len(), 5);
        // The assistant message survives as a normal end…
        let AgentMessage::Assistant(assistant) = &repaired[1] else {
            panic!("expected the assistant message");
        };
        assert_eq!(assistant.stop_reason, StopReason::Stop);
        assert_eq!(assistant.error_message, None);
        // …and each call gets exactly one synthetic error result, in call
        // order, under its own tool name, stating the user interrupted.
        assert_eq!(result_ids(&repaired), ["call-1", "call-2", "call-3"]);
        for (message, name) in repaired[2..].iter().zip(["bash", "read", "edit"]) {
            let AgentMessage::ToolResult(result) = message else {
                panic!("expected synthetic results");
            };
            assert!(result.is_error);
            assert_eq!(result.tool_name, name);
            assert!(result_text(result).contains("interrupted by the user"));
        }
    }

    #[test]
    fn repair_keeps_results_that_arrived_and_sweeps_only_the_rest() {
        // Interrupted mid-execution: call-1 ran, call-2 never did.
        let messages = vec![
            user("do two things"),
            AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![
                    tool_call_block("call-1", "bash"),
                    tool_call_block("call-2", "read"),
                ],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            })),
            tool_result("call-1"),
        ];
        let repaired = repair_history(&messages);
        assert_eq!(repaired.len(), 4);
        assert_eq!(result_ids(&repaired), ["call-1", "call-2"]);
        let AgentMessage::ToolResult(synthetic) = &repaired[3] else {
            panic!("expected the synthetic result");
        };
        assert_eq!(synthetic.tool_call_id, "call-2");
        assert!(synthetic.is_error);
    }

    #[test]
    fn an_aborted_message_with_no_content_is_dropped() {
        let messages = vec![
            user("hello"),
            terminal_assistant(StopReason::Aborted, vec![]),
        ];
        assert_eq!(repair_history(&messages), vec![user("hello")]);
    }

    #[test]
    fn synthetic_results_sit_with_the_call_not_at_the_tail() {
        // The interrupted-mid-batch shape: call-1 got its aborted result,
        // the loop then emitted an empty aborted assistant, and the chat
        // went on. call-2's synthetic result must follow call-1's, not
        // trail the later messages.
        let messages = vec![
            user("do two things"),
            AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![
                    tool_call_block("call-1", "bash"),
                    tool_call_block("call-2", "read"),
                ],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            })),
            tool_result("call-1"),
            terminal_assistant(StopReason::Aborted, vec![]),
            user("never mind"),
            assistant("ok"),
        ];
        let repaired = repair_history(&messages);
        assert_eq!(result_ids(&repaired), ["call-1", "call-2"]);
        assert!(matches!(&repaired[2], AgentMessage::ToolResult(r) if r.tool_call_id == "call-1"));
        assert!(matches!(&repaired[3], AgentMessage::ToolResult(r) if r.tool_call_id == "call-2"));
        assert_eq!(repaired[4], user("never mind"));
        assert_eq!(repaired.len(), 6);
    }

    #[test]
    fn an_errored_message_is_dropped_when_empty_and_kept_when_partial() {
        // No content: the question stays, the failed answer goes — the
        // Transcript keeps the visible error.
        let messages = vec![
            user("try this"),
            terminal_assistant(StopReason::Error, vec![]),
        ];
        assert_eq!(repair_history(&messages), vec![user("try this")]);

        // Partial content before the error: the interrupted rule applies —
        // the content survives as a normal end, the error does not.
        let messages = vec![
            user("try this"),
            terminal_assistant(StopReason::Error, vec![text_block("half an a")]),
        ];
        let repaired = repair_history(&messages);
        assert_eq!(repaired.len(), 2);
        let AgentMessage::Assistant(assistant) = &repaired[1] else {
            panic!("expected the assistant message");
        };
        assert_eq!(assistant.stop_reason, StopReason::Stop);
        assert_eq!(assistant.error_message, None);
    }

    #[test]
    fn a_load_with_a_dangling_tail_returns_a_repaired_history() {
        // The crash-mid-run shape on disk: the assistant tool-call message
        // landed, its results did not.
        let dir = temp_dir();
        append_message(&dir, "chat-1", &user("go")).unwrap();
        append_message(
            &dir,
            "chat-1",
            &AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![tool_call_block("call-1", "bash")],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            })),
        )
        .unwrap();
        let replayed = load(&dir, "chat-1").unwrap();
        assert_eq!(replayed.len(), 2);
        let repaired = load_repaired(&dir, "chat-1").unwrap();
        assert_eq!(result_ids(&repaired), ["call-1"]);
        assert_eq!(repaired.len(), 3);

        // The repair is on disk too: a later Turn and another load keep the
        // synthetic result next to its call instead of re-deriving it at
        // the tail behind the newer messages.
        append_message(&dir, "chat-1", &user("later")).unwrap();
        append_message(&dir, "chat-1", &assistant("sure")).unwrap();
        let reloaded = load_repaired(&dir, "chat-1").unwrap();
        assert_eq!(reloaded.len(), 5);
        assert!(matches!(&reloaded[2], AgentMessage::ToolResult(r) if r.tool_call_id == "call-1"));
        assert_eq!(reloaded[3], user("later"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_gets_its_header_on_the_first_append() {
        // The crash-between-create-and-header shape.
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(dir.join("history/chat-1.jsonl"), "").unwrap();
        append_message(&dir, "chat-1", &user("hello")).unwrap();
        assert_eq!(load(&dir, "chat-1").unwrap(), vec![user("hello")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quarantine_never_overwrites_an_earlier_quarantine() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        for _ in 0..2 {
            std::fs::write(dir.join("history/chat-1.jsonl"), "garbage\n").unwrap();
            quarantine(&dir, "chat-1");
        }
        let aside: Vec<_> = dir
            .join("history")
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".corrupt"))
            .collect();
        assert_eq!(aside.len(), 2, "{aside:?}");
        assert!(!dir.join("history/chat-1.jsonl").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("holt-history-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn round_trips_messages_behind_a_version_header() {
        let dir = temp_dir();
        append_message(&dir, "chat-1", &user("hello")).unwrap();
        append_message(&dir, "chat-1", &assistant("hi there")).unwrap();
        append_message(&dir, "chat-1", &user("again")).unwrap();
        assert_eq!(
            load(&dir, "chat-1").unwrap(),
            vec![user("hello"), assistant("hi there"), user("again")]
        );

        // The first line is the version header; every line after it is one
        // JSON object per entry.
        let text = std::fs::read_to_string(dir.join("history/chat-1.jsonl")).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], serde_json::json!(1));
        let entries: Vec<serde_json::Value> =
            lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|entry| entry["kind"] == "message"));
        // The message rides in its upstream serde shape: a user entry keeps
        // its role field verbatim.
        assert_eq!(entries[0]["entry"]["role"], "user");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_or_unsafe_chat_id_loads_empty_and_writes_nothing() {
        let dir = temp_dir();
        assert!(load(&dir, "never-ran").unwrap().is_empty());
        // Path-hostile ids neither read nor write outside the history dir.
        append_message(&dir, "../escape", &user("x")).unwrap();
        assert!(load(&dir, "../escape").unwrap().is_empty());
        assert!(!dir.join("escape.jsonl").exists());
        match dir.join("history").read_dir() {
            Ok(mut entries) => assert!(entries.next().is_none()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("unreadable history dir: {error}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_entry_kinds_and_truncated_tails_are_tolerated() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(
            dir.join("history/chat-1.jsonl"),
            concat!(
                "{\"version\":1}\n",
                "{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"before\",\"timestamp\":1}}\n",
                "{\"kind\":\"some-future-kind\",\"entry\":{}}\n",
                "{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"after\",\"timestamp\":2}}\n",
                // A trailing line cut mid-object — the crash-mid-append
                // shape — is treated as absent.
                "{\"kind\":\"mess",
            ),
        )
        .unwrap();

        let loaded = load(&dir, "chat-1").unwrap();
        let texts: Vec<&str> = loaded
            .iter()
            .filter_map(|message| match message {
                AgentMessage::User(message) => match &message.content {
                    UserContent::Text(text) => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["before", "after"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_version_is_an_error_not_a_silent_wipe() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(
            dir.join("history/chat-1.jsonl"),
            "{\"version\":99}\n{\"kind\":\"message\",\"entry\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":1}}\n",
        )
        .unwrap();
        assert!(load(&dir, "chat-1").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_only_that_chats_history() {
        let dir = temp_dir();
        append_message(&dir, "chat-1", &user("x")).unwrap();
        append_message(&dir, "chat-2", &user("y")).unwrap();
        delete_history(&dir, "chat-1");
        assert!(load(&dir, "chat-1").unwrap().is_empty());
        assert_eq!(load(&dir, "chat-2").unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
