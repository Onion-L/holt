//! The agent-event → transcript translation: pi-core tool calls and
//! assistant messages decode into `MessagePart`s, completed results stamp
//! onto their chips wherever they sit, and every completed unit lands its
//! incremental line in the log (ADR-0032).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use holt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, sanitize_tool_call};
use holt_proto::ToolCall as TranscriptToolCall;
use pi_core::{
    agent::types::{AgentMessage, AgentToolResult},
    ai::types::{AssistantContent, BlockContent},
};

use crate::agent::ChatRuntime;
use crate::history::CompactionRecord;

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
        "read_chat" => TranscriptToolCall::ReadChat {
            chat_id: arg("url")
                .as_deref()
                .and_then(|url| holt_proto::parse_holt_chat_link(url).ok())
                .map(|link| link.chat_id)
                .unwrap_or_else(|| "invalid Chat link".into()),
            title: None,
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
        "ls" => TranscriptToolCall::ListDir { path: arg("path") },
        // ADR-0023: the web tools decode onto the sync-era chips. `prompt`
        // is never populated — full-text fetch has no summarizer — and the
        // search chip carries only its query.
        "web_fetch" => TranscriptToolCall::WebFetch {
            url: arg("url").unwrap_or_default(),
            prompt: None,
        },
        "web_search" => TranscriptToolCall::WebSearch {
            query: arg("query").unwrap_or_default(),
        },
        // MCP tools (ADR-0034) decode onto the structured Mcp chip: the
        // two-level name splits at the first `__` after the prefix, the
        // input rides verbatim. A server name containing `__` would
        // mis-split the DISPLAY only — the full name stays authoritative
        // in History and approval grants.
        name if let Some(rest) = name.strip_prefix("mcp__") => {
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            TranscriptToolCall::Mcp {
                server: server.to_owned(),
                tool: tool.to_owned(),
                input: Some(serde_json::Value::Object(arguments.clone())),
            }
        }
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
pub(crate) fn tool_output_full(result: &AgentToolResult) -> Option<String> {
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

pub(crate) fn tool_usage_total(result: &AgentToolResult) -> Option<u64> {
    if let Some(usage) = result.usage.as_ref() {
        return Some(crate::usage::gross_tokens(usage));
    }
    // Older history records may only retain the serialized details payload.
    // Accept both the typed result and that wire-shaped fallback.
    let usage = result.details.get("usage")?.as_object()?;
    let token = |name: &str| {
        usage
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let total = token("input") + token("output") + token("cacheRead") + token("cacheWrite");
    Some(total)
}

/// Stamp a tool result onto the matching Tool part, wherever its entry sits.
pub(crate) fn resolve_tool_part(
    chat: &ChatRuntime,
    tool_call_id: &str,
    is_error: bool,
    output: Option<String>,
    read_chat_title: Option<&str>,
    subagent_usage: Option<u64>,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed: Option<(String, usize)> = None;
    for entry in transcript.iter_mut() {
        let hit =
            entry.parts.iter_mut().enumerate().find(
                |(_, part)| matches!(part, MessagePart::Tool { id, .. } if id == tool_call_id),
            );
        if let Some((
            tool_index,
            MessagePart::Tool {
                call,
                resolved,
                is_error: part_error,
                output: part_output,
                subagent_usage: usage_slot,
                ..
            },
        )) = hit
        {
            *resolved = true;
            *part_error = is_error;
            *part_output = output.clone();
            if call.is_subagent_spawn() {
                *usage_slot = subagent_usage;
            }
            if let TranscriptToolCall::ReadChat { title, .. } = call {
                *title = read_chat_title.map(str::to_owned);
            }
            changed = Some((entry.id.clone(), tool_index));
        }
        if changed.is_some() {
            break;
        }
    }
    drop(transcript);
    // A completed tool call is a completed unit (ADR-0032 mirrors
    // ADR-0010's per-message granularity): the round's new parts and the
    // result line land now, so a crash mid-Turn keeps every finished round
    // on disk — incrementally, not as a whole-entry re-append.
    if let Some((entry_id, tool_index)) = changed {
        chat.persist_tool_result(&entry_id, tool_index);
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
            id: entry_id.clone(),
            role: MessageRole::System,
            parts: vec![part],
            created_at: Utc::now().timestamp_millis(),
            device_id: device_id.to_string(),
            status: None,
            continuation_of: None,
        });
    // A housekeeping entry is complete the moment it is built — it lands
    // in the log now, not on a later publish (ADR-0032).
    chat.persist_entry(&entry_id);
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
    if chat.is_removed() {
        return;
    }
    chat.append_compaction(record);
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
pub(crate) fn record_mid_turn_compaction(
    chat: &ChatRuntime,
    record: &CompactionRecord,
    run_base_parts: &Arc<Mutex<Vec<MessagePart>>>,
) {
    if chat.is_removed() {
        return;
    }
    chat.append_compaction(record);
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

/// A run that ends early (abort, loop error) leaves tool parts without their
/// results; settle them so no chip stays "in call" forever.
pub(crate) fn settle_unresolved_tools(chat: &ChatRuntime) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    let mut changed = Vec::new();
    for entry in transcript.iter_mut() {
        let mut entry_changed = false;
        for part in entry.parts.iter_mut() {
            if let MessagePart::Tool { resolved, .. } = part
                && !*resolved
            {
                *resolved = true;
                entry_changed = true;
            }
        }
        if entry_changed {
            changed.push(entry.id.clone());
        }
    }
    drop(transcript);
    // The run's entry may already have landed in the log (the settle pass
    // runs first); re-append it so the settled chips persist too
    // (ADR-0032).
    for entry_id in changed {
        chat.persist_entry(&entry_id);
    }
}

/// char length of a part for the run-cadence debug trace.
pub(crate) fn part_char_len(part: &MessagePart) -> usize {
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
/// `cancelled` is the run's own cancellation (Stop / Steer). A cancelled run
/// ends on an aborted assistant message whose `error_message` is the
/// transport's abort artifact ("The operation was aborted"), not a provider
/// failure — like the loop-error path, it must not become an ErrorChip. An
/// abort WITHOUT cancellation (a transport dying on its own) keeps the chip.
///
/// `skill_files` maps this run's catalog `SKILL.md` paths (normalized
/// absolute) to skill names: a read of one collapses to the same skill chip
/// an invocation uses (ADR-0006) — the file's content reached the model
/// context through the tool result, and never enters the transcript.
pub(crate) fn assistant_parts(
    message: &AgentMessage,
    id_base: usize,
    cwd: &str,
    skill_files: &HashMap<String, String>,
    cancelled: bool,
) -> Vec<MessagePart> {
    let AgentMessage::Assistant(message) = message else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            AssistantContent::Text(text) => {
                // `<proposed_plan>` blocks fold into approval cards; the
                // surrounding text stays prose. Ids keep the running
                // offset so entry keys stay unique across messages.
                let base = id_base + parts.len();
                let mut n = 0usize;
                parts.extend(crate::plan_mode::plan_aware_text_parts(
                    &text.text,
                    &mut || {
                        n += 1;
                        format!("t{}", base + n - 1)
                    },
                ));
            }
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
                        subagent_usage: None,
                        gate: None,
                    });
                }
            }
            AssistantContent::Thinking(_) => {}
        }
    }
    if let Some(error) = message.error_message.as_ref() {
        let user_aborted =
            cancelled && message.stop_reason == pi_core::ai::types::StopReason::Aborted;
        if !user_aborted {
            parts.push(MessagePart::Error {
                id: format!("e{}", id_base + parts.len()),
                message: error.clone(),
            });
        }
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
pub(crate) fn update_assistant_entry(
    chat: &ChatRuntime,
    entry_id: &str,
    parts: Vec<MessagePart>,
    status: MessageStatus,
    device_id: &str,
    publish: bool,
) {
    let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = transcript.iter_mut().find(|entry| entry.id == entry_id) {
        let mut parts = parts;
        crate::provider_mode::carry_card_states(&existing.parts, &mut parts);
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
    // A terminal write is the entry's one landing in the log (ADR-0032);
    // while it streams, the entry lives in memory and the watch only.
    if status == MessageStatus::Streaming {
        if publish {
            chat.publish();
        }
    } else {
        chat.persist_entry(entry_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{
        AssistantContent, AssistantMessage, TextContent, ThinkingContent, ToolCall,
    };

    fn write_skill(root: &std::path::Path, name: &str, frontmatter: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\nbody\n"),
        )
        .unwrap();
    }

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
            assistant_parts(&message, 0, "/tmp/x", &HashMap::new(), false),
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
            assistant_parts(&message, 2, "/tmp/x", &HashMap::new(), false),
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
    fn a_cancelled_runs_aborted_message_drops_the_abort_error_part() {
        let aborted = || {
            AgentMessage::Assistant(Box::new(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "partial".into(),
                    ..Default::default()
                })],
                stop_reason: pi_core::ai::types::StopReason::Aborted,
                error_message: Some("The operation was aborted".into()),
                ..Default::default()
            }))
        };
        // Stop / Steer: the transport's abort artifact is not an error —
        // only the partial text lands.
        assert_eq!(
            assistant_parts(&aborted(), 0, "/tmp/x", &HashMap::new(), true),
            vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial".into(),
            }]
        );
        // An abort WITHOUT the run's cancellation (the transport died on
        // its own) keeps the chip — that failure must stay diagnosable.
        assert_eq!(
            assistant_parts(&aborted(), 0, "/tmp/x", &HashMap::new(), false),
            vec![
                MessagePart::Text {
                    id: "t0".into(),
                    text: "partial".into(),
                },
                MessagePart::Error {
                    id: "e1".into(),
                    message: "The operation was aborted".into(),
                },
            ]
        );
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
            assistant_parts(&message, 0, "/tmp/x", &HashMap::new(), false),
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
                subagent_usage: None,
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
        assert_eq!(
            transcript_tool_call(&tool_call(
                "read_chat",
                serde_json::json!({
                    "url": "holt://open/chat/chat-2?workspace=workspace"
                })
            )),
            TranscriptToolCall::ReadChat {
                chat_id: "chat-2".into(),
                title: None,
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
        // MCP two-level names decode onto the structured Mcp chip; like
        // every chip, the sanitize pass drops the input (it stays in
        // History and the expandable detail's live journal).
        assert_eq!(
            transcript_tool_call(&tool_call(
                "mcp__dashboard-icons__suggest_icon",
                serde_json::json!({ "query": "home" })
            )),
            TranscriptToolCall::Mcp {
                server: "dashboard-icons".into(),
                tool: "suggest_icon".into(),
                input: None,
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
        // ADR-0023: the two web tools fold onto the sync-era chips. A
        // `prompt` argument is dropped — full-text fetch never summarizes,
        // and the field stays `None` forever.
        assert_eq!(
            transcript_tool_call(&tool_call(
                "web_fetch",
                serde_json::json!({ "url": "https://example.test/page", "prompt": "summarize" })
            )),
            TranscriptToolCall::WebFetch {
                url: "https://example.test/page".into(),
                prompt: None,
            }
        );
        // Only the query rides the chip; `max_results` has no slot.
        assert_eq!(
            transcript_tool_call(&tool_call(
                "web_search",
                serde_json::json!({ "query": "holt", "max_results": 3 })
            )),
            TranscriptToolCall::WebSearch {
                query: "holt".into(),
            }
        );
        // Unknown tools degrade to a named chip, input intact (the policy
        // strips non-spawn inputs).
        let decoded = transcript_tool_call(&tool_call(
            "whats_new",
            serde_json::json!({ "query": "holt" }),
        ));
        assert!(matches!(
            decoded,
            TranscriptToolCall::Unknown { ref name, input: None } if name == "whats_new"
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
            assistant_parts(&message, 0, "/roots", &skill_files, false)
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
                subagent_usage: None,
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
        let parts = assistant_parts(&message, 0, "/roots", &HashMap::new(), false);
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
                subagent_usage: None,
                gate: None,
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        });
        resolve_tool_part(&chat, "call-9", true, Some("boom".into()), None, None);
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
        let mut first_parts = assistant_parts(&text("hello"), 0, "/tmp/x", &HashMap::new(), false);
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
            false,
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
