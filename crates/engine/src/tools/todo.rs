//! The agent's task list (issue #47): a TodoWrite-style `todo` tool that
//! lets the model maintain a structured checklist during a Turn — items
//! with a short title and a status (`pending` / `in_progress` / `done` /
//! `cancelled`). The tool keeps no state: every call carries the whole
//! list, and the transcript's card (landed via the result details, the
//! `choose_provider` pattern) is the record — one card per Turn, rewritten
//! in place on each call, so multi-step Turns show their own progress
//! instead of narrating it in prose. Chat-scoped display state only: the
//! tool never touches the workspace filesystem, and `todo` is not a
//! mutating name — it never meets the permission gate
//! (ADR-permission-modes-gate-mutations-no-os-sandbox).

use futures::future::BoxFuture;
use holt_doc::MessagePart;
use holt_doc::parts::{TodoCardItem, TodoItemStatus};
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::ChatRuntime;

/// The list's bounds (issue #47: "bounded like other bounded surfaces"):
/// a cap hit is a typed tool error the model reads, never a silent
/// truncation.
pub(crate) const MAX_ITEMS: usize = 20;
pub(crate) const MAX_TITLE_CHARS: usize = 200;

const DESCRIPTION: &str = "Maintain this turn's task list: a checklist capsule pinned above the \
transcript that updates in place as work progresses. Use this tool proactively for any multi-step work \
(roughly 3+ steps): create the list BEFORE starting the work, keep exactly one item \
`in_progress`, and mark each item `done` the moment its step finishes — the user follows the \
capsule, so update it rather than narrating progress in prose. Skip it for trivial or single-step \
tasks. Each call REPLACES the whole list — carry every item forward with its current status \
(`pending`, `in_progress`, `done`, `cancelled`). Mark an item `done` only when its work is \
actually finished; cancel rather than delete work that will not happen. Keep titles short and \
imperative. Bounded: at most 20 items and 200 characters per title — a cap hit is an error, so \
trim the list instead of overflowing it.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemInput {
    title: String,
    status: String,
}

fn parse_status(raw: &str, title: &str) -> Result<TodoItemStatus, String> {
    match raw {
        "pending" => Ok(TodoItemStatus::Pending),
        "in_progress" => Ok(TodoItemStatus::InProgress),
        "done" => Ok(TodoItemStatus::Done),
        "cancelled" => Ok(TodoItemStatus::Cancelled),
        other => Err(format!(
            "\"{title}\" has unknown status \"{other}\" — one of \
             pending / in_progress / done / cancelled"
        )),
    }
}

/// Validate and normalize one write. A cap hit is a typed error — the
/// list the model gets back is always the list it asked for, or none.
fn validate(inputs: &[ItemInput]) -> Result<Vec<TodoCardItem>, String> {
    if inputs.is_empty() {
        return Err(
            "pass at least one item — the list replaces whole on each call; a Turn with \
             nothing tracked simply never calls todo"
                .into(),
        );
    }
    if inputs.len() > MAX_ITEMS {
        return Err(format!(
            "at most {MAX_ITEMS} items per list (got {}) — merge or drop the least \
             valuable instead of overflowing",
            inputs.len()
        ));
    }
    let mut in_progress = 0usize;
    let mut items = Vec::with_capacity(inputs.len());
    for input in inputs {
        let title = input.title.trim();
        if title.is_empty() {
            return Err("every item needs a non-empty title".into());
        }
        if title.chars().count() > MAX_TITLE_CHARS {
            return Err(format!(
                "titles cap at {MAX_TITLE_CHARS} characters — \"{}\" runs long",
                title.chars().take(40).collect::<String>()
            ));
        }
        let status = parse_status(input.status.trim(), title)?;
        if status == TodoItemStatus::InProgress {
            in_progress += 1;
        }
        items.push(TodoCardItem {
            title: title.to_string(),
            status,
        });
    }
    if in_progress > 1 {
        return Err("at most one item may be in_progress — finish or pause the others".into());
    }
    Ok(items)
}

/// The ack the model reads back: quiet counts, not a re-print of the list
/// it just wrote.
fn ack_text(items: &[TodoCardItem]) -> String {
    let done = items
        .iter()
        .filter(|item| item.status == TodoItemStatus::Done)
        .count();
    let working = items
        .iter()
        .filter(|item| item.status == TodoItemStatus::InProgress)
        .count();
    let cancelled = items
        .iter()
        .filter(|item| item.status == TodoItemStatus::Cancelled)
        .count();
    let mut text = format!("Task list updated — {done} of {} done", items.len());
    if working > 0 {
        text.push_str(", 1 in progress");
    }
    if cancelled > 0 {
        text.push_str(&format!(", {cancelled} cancelled"));
    }
    text.push('.');
    text
}

fn todo_write(params: &serde_json::Value) -> Result<AgentToolResult, String> {
    let inputs: Vec<ItemInput> = serde_json::from_value(
        params
            .get("items")
            .cloned()
            .ok_or("pass \"items\" — the whole list, each {title, status}")?,
    )
    .map_err(|error| format!("invalid item shape: {error}"))?;
    let items = validate(&inputs)?;
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text: ack_text(&items),
            ..Default::default()
        })],
        details: json!({ "items": items }),
        ..Default::default()
    })
}

pub(crate) fn create_todo_tool() -> AgentTool {
    AgentTool {
        name: "todo".into(),
        label: "Todo".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "maxLength": MAX_TITLE_CHARS,
                                "description": "A short imperative title for the task"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "cancelled"],
                                "description": "Where the task stands"
                            }
                        },
                        "required": ["title", "status"],
                        "additionalProperties": false
                    },
                    "minItems": 1,
                    "maxItems": MAX_ITEMS,
                    "description": "The whole task list, replacing the previous one"
                }
            },
            "required": ["items"],
            "additionalProperties": false
        }),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(
            move |_tool_call_id: &str,
                  params: &serde_json::Value,
                  signal: Option<&CancellationToken>,
                  _on_update: Option<&AgentToolUpdateCallback>| {
                let result = if signal.is_some_and(CancellationToken::is_cancelled) {
                    Err("todo cancelled".into())
                } else {
                    todo_write(params)
                };
                Box::pin(async move { result })
                    as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// The items a settled `todo` result carries, parsed back for the card.
/// `None` on a shape the tool itself would never write (an errored call
/// never reaches here — the caller gates on `!is_error`).
pub(crate) fn card_items(details: &serde_json::Value) -> Option<Vec<TodoCardItem>> {
    let items = details.get("items")?.as_array()?;
    let items: Vec<TodoCardItem> = items
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    (!items.is_empty()).then_some(items)
}

/// Land the write on the Turn's card: the FIRST `todo` call of a Turn
/// appends the card after its tool chip; every later call rewrites that
/// same card in place — the list updates where it landed instead of
/// stacking one card per call. Persisted as it changes: a fresh card rides
/// the entry's tail append; a rewrite of an already-landed card backfills
/// it with an in-place `part` line (ADR-transcript-append-only-log-per-chat-lock).
pub(crate) fn upsert_card(
    chat: &ChatRuntime,
    base_parts: &Arc<std::sync::Mutex<Vec<MessagePart>>>,
    run_entry: &str,
    tool_call_id: &str,
    details: &serde_json::Value,
) {
    let Some(items) = card_items(details) else {
        return;
    };
    let card_id = || format!("{tool_call_id}-card");
    {
        let mut base = base_parts.lock().unwrap_or_else(|e| e.into_inner());
        match base.iter_mut().find_map(|part| match part {
            MessagePart::TodoCard { items: slot, .. } => Some(slot),
            _ => None,
        }) {
            Some(slot) => *slot = items.clone(),
            None => base.push(MessagePart::TodoCard {
                id: card_id(),
                items: items.clone(),
            }),
        }
    }
    let mut card_index = None;
    {
        let mut transcript = chat.transcript.write().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = transcript.iter_mut().find(|entry| entry.id == run_entry) {
            match entry
                .parts
                .iter_mut()
                .enumerate()
                .find_map(|(ix, part)| match part {
                    MessagePart::TodoCard { items: slot, .. } => Some((ix, slot)),
                    _ => None,
                }) {
                Some((ix, slot)) => {
                    *slot = items;
                    card_index = Some(ix);
                }
                None => {
                    entry.parts.push(MessagePart::TodoCard {
                        id: card_id(),
                        items,
                    });
                    card_index = Some(entry.parts.len() - 1);
                }
            }
        }
    }
    if let Some(ix) = card_index {
        // The completed-unit writer: tail parts first, then the card's
        // in-place backfill when it predates them.
        chat.persist_tool_result(run_entry, ix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_doc::{MessageRole, SessionMessageEntry};

    fn write(items: serde_json::Value) -> Result<AgentToolResult, String> {
        todo_write(&json!({ "items": items }))
    }

    #[test]
    fn a_valid_write_normalizes_and_reports_the_list() {
        let result = write(json!([
            { "title": "  Map the fold path  ", "status": "done" },
            { "title": "Ship it", "status": "in_progress" },
            { "title": "Cancel the footer rework", "status": "cancelled" }
        ]))
        .unwrap();
        assert_eq!(
            result.details,
            json!({
                "items": [
                    { "title": "Map the fold path", "status": "done" },
                    { "title": "Ship it", "status": "inProgress" },
                    { "title": "Cancel the footer rework", "status": "cancelled" }
                ]
            })
        );
        let BlockContent::Text(text) = &result.content[0] else {
            panic!("expected a text ack");
        };
        assert_eq!(
            text.text,
            "Task list updated — 1 of 3 done, 1 in progress, 1 cancelled."
        );
    }

    #[test]
    fn rejects_empty_oversized_and_unknown_shapes() {
        // No items key / empty list.
        assert!(todo_write(&json!({})).is_err());
        let error = write(json!([])).unwrap_err();
        assert!(error.contains("at least one item"), "{error}");
        // Over the item cap — a typed error, not a silent truncation.
        let fat = (0..MAX_ITEMS + 1)
            .map(|i| json!({ "title": format!("task {i}"), "status": "pending" }))
            .collect::<Vec<_>>();
        let error = write(json!(fat)).unwrap_err();
        assert!(error.contains("at most 20 items"), "{error}");
        // Over the title cap.
        let error = write(json!([{ "title": "x".repeat(201), "status": "pending" }])).unwrap_err();
        assert!(error.contains("200 characters"), "{error}");
        // Blank title, unknown status, stray field, missing status.
        assert!(write(json!([{ "title": "  ", "status": "pending" }])).is_err());
        let error = write(json!([{ "title": "x", "status": "doing" }])).unwrap_err();
        assert!(error.contains("unknown status"), "{error}");
        assert!(write(json!([{ "title": "x", "status": "done", "note": "n" }])).is_err());
        assert!(write(json!([{ "title": "x" }])).is_err());
    }

    #[test]
    fn at_most_one_item_in_progress() {
        assert!(
            write(json!([
                { "title": "a", "status": "in_progress" },
                { "title": "b", "status": "in_progress" }
            ]))
            .is_err()
        );
        // Zero is fine — a paused or freshly-planned list.
        assert!(
            write(json!([
                { "title": "a", "status": "done" },
                { "title": "b", "status": "pending" }
            ]))
            .is_ok()
        );
    }

    fn chat_with_entry(entry_id: &str) -> ChatRuntime {
        let chat = ChatRuntime::new();
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: entry_id.into(),
            role: MessageRole::Assistant,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        });
        chat
    }

    fn card_of(chat: &ChatRuntime, entry_id: &str) -> Vec<TodoCardItem> {
        let transcript = chat.transcript.read().unwrap();
        let entry = transcript
            .iter()
            .find(|entry| entry.id == entry_id)
            .unwrap();
        entry
            .parts
            .iter()
            .find_map(|part| match part {
                MessagePart::TodoCard { items, .. } => Some(items.clone()),
                _ => None,
            })
            .expect("a todo card on the entry")
    }

    #[test]
    fn the_card_appends_once_then_rewrites_in_place() {
        let chat = chat_with_entry("run-1");
        let base = Arc::new(std::sync::Mutex::new(Vec::new()));
        let first = json!({ "items": [
            { "title": "one", "status": "inProgress" },
            { "title": "two", "status": "pending" }
        ] });
        upsert_card(&chat, &base, "run-1", "call-1", &first);
        assert_eq!(card_of(&chat, "run-1").len(), 2);
        assert_eq!(base.lock().unwrap().len(), 1);

        // The second write keeps ONE card, at its landing spot, carrying
        // the new state — and the base (what streaming rebuilds draw from)
        // agrees.
        let second = json!({ "items": [
            { "title": "one", "status": "done" },
            { "title": "two", "status": "inProgress" },
            { "title": "three", "status": "pending" }
        ] });
        upsert_card(&chat, &base, "run-1", "call-2", &second);
        let transcript = chat.transcript.read().unwrap();
        let cards = transcript[0]
            .parts
            .iter()
            .filter(|part| matches!(part, MessagePart::TodoCard { .. }))
            .count();
        assert_eq!(cards, 1, "one card per Turn, rewritten in place");
        drop(transcript);
        let items = card_of(&chat, "run-1");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoItemStatus::Done);
        assert_eq!(items[1].status, TodoItemStatus::InProgress);
        let MessagePart::TodoCard { id, items } = &base.lock().unwrap()[0] else {
            panic!("the base keeps the card");
        };
        // The first call's id sticks — the card's row identity is stable.
        assert_eq!(id, "call-1-card");
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn a_shape_the_tool_never_wrote_lands_no_card() {
        let chat = chat_with_entry("run-1");
        let base = Arc::new(std::sync::Mutex::new(Vec::new()));
        upsert_card(&chat, &base, "run-1", "call-1", &json!({}));
        upsert_card(&chat, &base, "run-1", "call-1", &json!({ "items": [] }));
        assert!(chat.transcript.read().unwrap()[0].parts.is_empty());
        assert!(base.lock().unwrap().is_empty());

        // A status this build has never heard of still lands — the card is
        // a display record read tolerantly (a downgrade keeps rendering),
        // never a decode failure.
        upsert_card(
            &chat,
            &base,
            "run-1",
            "call-1",
            &json!({ "items": [{ "title": "x", "status": "nope" }] }),
        );
        assert_eq!(card_of(&chat, "run-1")[0].status, TodoItemStatus::Unknown);
    }
}
