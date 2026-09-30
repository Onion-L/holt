//! The agent's question card (ADR-0040): `ask_user` asks the user one
//! question with 2–6 enumerated options. The call never waits — it lands a
//! `MessagePart::QuestionCard` in the transcript (via its result details,
//! the `choose_provider` pattern) and returns; the model stops its Turn.
//! The user clicks an option or types an answer, `SettleQuestion` stamps
//! the card and queues the answer as an ordinary user message, so the
//! next Turn reads it from the conversation — no blocking tool result, no
//! pending-RPC machinery, restart-safe by construction.

use futures::future::BoxFuture;
use holt_doc::parts::{ChoiceCardState, MessagePart};
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::ChatRuntime;

const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 6;

const DESCRIPTION: &str = "Ask the user ONE question when the answer gates your next step and \
the choices can be enumerated as 2–6 concrete options. Pass the question and the options, \
most likely first. A question card appears in the conversation; the user clicks an option or \
types an answer, and it arrives as the next user message. Do NOT also ask in text; STOP your \
turn after calling — do not call other tools afterwards. Open-ended questions that cannot be \
enumerated stay in ordinary text.";

/// The queued answer (ADR-0040): an ordinary user message carrying the
/// question it answers, so the next Turn reads both from History.
pub(crate) fn question_answer_notice(question: &str, answer: &str) -> String {
    format!("To your question \"{question}\": {answer}")
}

fn text_result(text: String, details: serde_json::Value) -> Result<AgentToolResult, String> {
    Ok(AgentToolResult {
        content: vec![BlockContent::Text(TextContent {
            text,
            ..Default::default()
        })],
        details,
        ..Default::default()
    })
}

fn ask_user(params: &serde_json::Value) -> Result<AgentToolResult, String> {
    let question = params
        .get("question")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .ok_or("pass a non-empty \"question\"")?
        .to_string();
    let options: Vec<String> = params
        .get("options")
        .and_then(serde_json::Value::as_array)
        .ok_or("pass \"options\" as an array of 2–6 strings")?
        .iter()
        .filter_map(|option| option.as_str())
        .map(str::trim)
        .filter(|option| !option.is_empty())
        .map(str::to_owned)
        .collect();
    if options.len() < MIN_OPTIONS {
        return Err(format!(
            "pass {MIN_OPTIONS}–{MAX_OPTIONS} non-empty options — an open-ended \
             question belongs in ordinary text"
        ));
    }
    if options.len() > MAX_OPTIONS {
        return Err(format!(
            "pass at most {MAX_OPTIONS} options — drop the least likely and put \
             the rest in order"
        ));
    }
    if options
        .iter()
        .any(|option| options.iter().filter(|o| o == &option).count() > 1)
    {
        return Err("pass distinct options".into());
    }
    text_result(
        "Question card shown. STOP this turn — the user answers on the card or \
         in chat, and the answer arrives as the next message."
            .into(),
        json!({ "question": question, "options": options }),
    )
}

pub(crate) fn create_ask_user_tool() -> AgentTool {
    AgentTool {
        name: "ask_user".into(),
        label: "Ask User".into(),
        description: DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "maxLength": 500,
                    "description": "The one question, asked verbatim on the card"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": MIN_OPTIONS,
                    "maxItems": MAX_OPTIONS,
                    "description": "The 2–6 concrete answers, most likely first"
                }
            },
            "required": ["question", "options"],
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
                    Err("ask_user cancelled".into())
                } else {
                    ask_user(params)
                };
                Box::pin(async move { result })
                    as BoxFuture<'static, Result<AgentToolResult, String>>
            },
        ),
    }
}

/// The card a settled `ask_user` call leaves in the transcript, built from
/// its result details — the same shape `provider_mode::tool_card` serves.
pub(crate) fn tool_card(
    tool_call_id: &str,
    tool_name: &str,
    details: &serde_json::Value,
) -> Option<MessagePart> {
    if tool_name != "ask_user" {
        return None;
    }
    let question = details.get("question")?.as_str()?.to_string();
    let options = details
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|option| option.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    (!question.is_empty() && !options.is_empty()).then(|| MessagePart::QuestionCard {
        id: format!("{tool_call_id}-card"),
        question,
        options,
        chosen: None,
        state: ChoiceCardState::Pending,
    })
}

/// Retire every pending question card: a newer question, or any new Turn —
/// a typed answer moved the conversation past it.
pub(crate) fn supersede_question_cards(chat: &ChatRuntime) {
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard { state, .. } if *state == ChoiceCardState::Pending => {
            *state = ChoiceCardState::Superseded;
            true
        }
        _ => false,
    });
}

/// Settle one pending question card on the user's answer — the click or
/// the typed text — and return (question, answer) for the queued message.
/// Free text is the point of the card, so the answer is taken verbatim;
/// only the card's identity is checked, under one transcript lock, so two
/// clicks cannot both settle it.
pub(crate) fn settle_question(
    chat: &ChatRuntime,
    card_id: &str,
    answer: &str,
) -> Result<(String, String), String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("the answer is empty".into());
    }
    let mut settled = None;
    let mut refusal = "no such question on this chat";
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard {
            id,
            question,
            chosen,
            state,
            ..
        } if id == card_id => {
            if *state != ChoiceCardState::Pending {
                refusal = "this question is already settled";
                return false;
            }
            *chosen = Some(answer.to_string());
            *state = ChoiceCardState::Chosen;
            settled = Some((question.clone(), answer.to_string()));
            true
        }
        _ => false,
    });
    settled.ok_or_else(|| refusal.to_string())
}

/// Put an answered card back to pending — the queued message did not go
/// out.
pub(crate) fn unsettle_question(chat: &ChatRuntime, card_id: &str) {
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard {
            id, chosen, state, ..
        } if id == card_id && *state == ChoiceCardState::Chosen => {
            *chosen = None;
            *state = ChoiceCardState::Pending;
            true
        }
        _ => false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_doc::{MessageRole, SessionMessageEntry};

    fn card(id: &str, state: ChoiceCardState) -> MessagePart {
        MessagePart::QuestionCard {
            id: id.into(),
            question: "Ship the retry as prefix or suffix?".into(),
            options: vec!["prefix".into(), "suffix".into()],
            chosen: None,
            state,
        }
    }

    fn chat_with(parts: Vec<MessagePart>) -> ChatRuntime {
        let chat = ChatRuntime::new();
        chat.transcript.write().unwrap().push(SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        });
        chat
    }

    fn transcript(chat: &ChatRuntime) -> Vec<MessagePart> {
        chat.transcript
            .read()
            .unwrap()
            .iter()
            .flat_map(|entry| entry.parts.iter().cloned())
            .collect()
    }

    fn state_of(part: &MessagePart) -> ChoiceCardState {
        match part {
            MessagePart::QuestionCard { state, .. } => *state,
            other => panic!("expected a question card, got {other:?}"),
        }
    }

    fn result_details(result: &AgentToolResult) -> &serde_json::Value {
        &result.details
    }

    #[test]
    fn a_valid_call_reports_options_for_the_card() {
        let result = ask_user(
            &json!({ "question": "  Prefix or suffix?  ", "options": ["prefix", "suffix"] }),
        )
        .unwrap();
        assert_eq!(
            result_details(&result),
            &json!({ "question": "Prefix or suffix?", "options": ["prefix", "suffix"] })
        );
    }

    #[test]
    fn rejects_empty_questions_and_thin_or_fat_option_lists() {
        assert!(ask_user(&json!({ "question": "", "options": ["a", "b"] })).is_err());
        assert!(ask_user(&json!({ "options": ["a", "b"] })).is_err());
        assert!(ask_user(&json!({ "question": "q", "options": ["only"] })).is_err());
        let seven = (0..7).map(|i| i.to_string()).collect::<Vec<_>>();
        assert!(ask_user(&json!({ "question": "q", "options": seven })).is_err());
        assert!(ask_user(&json!({ "question": "q", "options": ["a", "a"] })).is_err());
    }

    #[test]
    fn tool_card_builds_only_from_an_ask_user_result() {
        let details = json!({ "question": "q?", "options": ["a", "b"] });
        let Some(MessagePart::QuestionCard { id, state, .. }) =
            tool_card("t1", "ask_user", &details)
        else {
            panic!("expected a question card");
        };
        assert_eq!(id, "t1-card");
        assert_eq!(state, ChoiceCardState::Pending);
        assert!(tool_card("t1", "bash", &details).is_none());
        assert!(tool_card("t1", "ask_user", &json!({})).is_none());
    }

    #[test]
    fn settle_stamps_under_a_lock_and_refuses_second_settles() {
        let chat = chat_with(vec![card("q1", ChoiceCardState::Pending)]);
        let (question, answer) =
            settle_question(&chat, "q1", "  suffix  ").expect("the pending card settles");
        assert_eq!(question, "Ship the retry as prefix or suffix?");
        assert_eq!(answer, "suffix");
        assert!(matches!(
            &transcript(&chat)[0],
            MessagePart::QuestionCard {
                chosen: Some(answer),
                state: ChoiceCardState::Chosen,
                ..
            } if answer == "suffix"
        ));
        assert_eq!(
            settle_question(&chat, "q1", "prefix").unwrap_err(),
            "this question is already settled"
        );
        assert_eq!(
            settle_question(&chat, "missing", "x").unwrap_err(),
            "no such question on this chat"
        );
        assert!(settle_question(&chat, "q1", "   ").is_err());
    }

    #[test]
    fn unsettle_restores_pending_for_the_retry() {
        let chat = chat_with(vec![card("q1", ChoiceCardState::Pending)]);
        settle_question(&chat, "q1", "suffix").unwrap();
        unsettle_question(&chat, "q1");
        assert!(matches!(
            &transcript(&chat)[0],
            MessagePart::QuestionCard {
                chosen: None,
                state: ChoiceCardState::Pending,
                ..
            }
        ));
    }

    #[test]
    fn a_new_turn_retires_only_still_pending_cards() {
        let chat = chat_with(vec![
            card("q1", ChoiceCardState::Pending),
            card("q2", ChoiceCardState::Chosen),
        ]);
        supersede_question_cards(&chat);
        let parts = transcript(&chat);
        assert_eq!(state_of(&parts[0]), ChoiceCardState::Superseded);
        assert_eq!(state_of(&parts[1]), ChoiceCardState::Chosen);
    }

    #[test]
    fn the_notice_reads_as_the_user_answering() {
        assert_eq!(
            question_answer_notice("Prefix or suffix?", "suffix"),
            "To your question \"Prefix or suffix?\": suffix"
        );
    }
}
