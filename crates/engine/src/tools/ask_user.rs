//! The agent's question card (ADR-0040): `ask_user` asks the user 1–4
//! questions, each with 2–6 enumerated options. The call never waits — it
//! lands a `MessagePart::QuestionCard` in the transcript (via its result
//! details, the `choose_provider` pattern) and returns; the model stops
//! its Turn. The user answers on the composer's approval bar, page by
//! page; `SettleQuestion` stamps the card and queues the answers as an
//! ordinary user message, so the next Turn reads them from the
//! conversation — no blocking tool result, no pending-RPC machinery,
//! restart-safe by construction.

use futures::future::BoxFuture;
use holt_doc::parts::{CardQuestion, ChoiceCardState, MessagePart};
use pi_core::{
    agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback},
    ai::types::{BlockContent, TextContent},
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::ChatRuntime;

const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 6;
const MAX_QUESTIONS: usize = 4;

const DESCRIPTION: &str = "Ask the user 1–4 questions when the answers gate your next step and \
each choice can be enumerated as 2–6 concrete options. Pass the questions with their options, \
most likely first. A question card appears in the conversation; the user answers page by page \
(by click or in words), and the answers arrive as the next user message. Do NOT also ask in \
text; STOP your turn after calling — do not call other tools afterwards. The user may \
dismiss the card unanswered; if the next Turn shows it superseded with no answers, move on \
or ask again in plain text — never re-call immediately. Open-ended questions that cannot \
be enumerated stay in ordinary text.";

/// The queued answers (ADR-0040): an ordinary user message carrying each
/// question it answers, so the next Turn reads both from History.
pub(crate) fn question_answer_notice(pairs: &[(String, String)]) -> String {
    match pairs {
        [(question, answer)] => format!("To your question \"{question}\": {answer}"),
        pairs => {
            let mut notice = String::from("To your questions:");
            for (question, answer) in pairs {
                notice.push_str(&format!("\n- \"{question}\": {answer}"));
            }
            notice
        }
    }
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionInput {
    question: String,
    options: Vec<String>,
}

fn validate_question(input: &QuestionInput) -> Result<CardQuestion, String> {
    let question = input.question.trim();
    if question.is_empty() {
        return Err("every question must be a non-empty string".into());
    }
    let options: Vec<String> = input
        .options
        .iter()
        .map(|option| option.trim().to_string())
        .filter(|option| !option.is_empty())
        .collect();
    if options.len() < MIN_OPTIONS {
        return Err(format!(
            "\"{question}\" needs {MIN_OPTIONS}–{MAX_OPTIONS} non-empty options — an \
             open-ended question belongs in ordinary text"
        ));
    }
    if options.len() > MAX_OPTIONS {
        return Err(format!(
            "\"{question}\" takes at most {MAX_OPTIONS} options — drop the least \
             likely and put the rest in order"
        ));
    }
    // "most likely first" is the contract: keep the caller's order and
    // check duplicates without sorting the stored options.
    let unique: HashSet<&str> = options.iter().map(String::as_str).collect();
    if unique.len() != options.len() {
        return Err(format!("\"{question}\" has duplicate options"));
    }
    Ok(CardQuestion {
        question: question.to_string(),
        options,
    })
}

fn ask_user(params: &serde_json::Value) -> Result<AgentToolResult, String> {
    let inputs: Vec<QuestionInput> = serde_json::from_value::<Vec<QuestionInput>>(
        params
            .get("questions")
            .cloned()
            .ok_or("pass \"questions\" — a list of 1–4 {question, options} objects")?,
    )
    .map_err(|error| format!("invalid question shape: {error}"))?;
    if inputs.is_empty() {
        return Err("pass at least one question".into());
    }
    if inputs.len() > MAX_QUESTIONS {
        return Err(format!(
            "ask at most {MAX_QUESTIONS} questions per call — split the rest across \
             the conversation"
        ));
    }
    let questions: Vec<CardQuestion> = inputs
        .iter()
        .map(validate_question)
        .collect::<Result<_, _>>()?;
    text_result(
        "Question card shown. STOP this turn — the user answers on the card or \
         in chat, and the answers arrive as the next message."
            .into(),
        json!({ "questions": questions }),
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
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "maxLength": 500,
                                "description": "The question, asked verbatim on the card"
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
                    },
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "description": "The questions to answer, most important first"
                }
            },
            "required": ["questions"],
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
    let questions: Vec<CardQuestion> = details
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|question| serde_json::from_value(question.clone()).ok())
        .collect();
    (!questions.is_empty()).then(|| MessagePart::QuestionCard {
        id: format!("{tool_call_id}-card"),
        questions,
        answers: Vec::new(),
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

/// Whether the chat's transcript holds a question card still waiting on
/// the user.
pub(crate) fn has_pending_question(chat: &ChatRuntime) -> bool {
    chat.transcript
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .flat_map(|entry| &entry.parts)
        .any(|part| {
            matches!(part, MessagePart::QuestionCard { state, .. }
                if *state == ChoiceCardState::Pending)
        })
}

/// Settle one pending question card on the user's answers — the pages of
/// the approval bar — and return the (question, answer) pairs for the
/// queued message. The answer count must match the questions; free text
/// is the point of the card, so answers are taken verbatim. Checked and
/// stamped under one transcript lock, so two answers cannot both settle
/// it.
pub(crate) fn settle_question(
    chat: &ChatRuntime,
    card_id: &str,
    answers: Vec<String>,
) -> Result<Vec<(String, String)>, String> {
    let answers: Vec<String> = answers
        .iter()
        .map(|answer| answer.trim().to_string())
        .collect();
    if answers.iter().any(|answer| answer.is_empty()) {
        return Err("every answer must be non-empty".into());
    }
    let mut settled = None;
    let mut refusal = "no such question on this chat";
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard {
            id,
            questions,
            answers: card_answers,
            state,
        } if id == card_id => {
            if *state != ChoiceCardState::Pending {
                refusal = "this question is already settled";
                return false;
            }
            if answers.len() != questions.len() {
                refusal = "the answers do not match the questions";
                return false;
            }
            let pairs = questions
                .iter()
                .zip(&answers)
                .map(|(question, answer)| (question.question.clone(), answer.clone()))
                .collect::<Vec<_>>();
            *card_answers = answers.clone();
            *state = ChoiceCardState::Chosen;
            settled = Some(pairs);
            true
        }
        _ => false,
    });
    settled.ok_or_else(|| refusal.to_string())
}

/// Retire one pending question card without an answer — the user
/// dismissed the bar. The model reads the unanswered Superseded card
/// next Turn. Refuses already-settled cards.
pub(crate) fn dismiss_question(chat: &ChatRuntime, card_id: &str) -> Result<(), String> {
    let mut dismissed = false;
    let mut refusal = "no such question on this chat";
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard { id, state, .. } if id == card_id => {
            if *state != ChoiceCardState::Pending {
                refusal = "this question is already settled";
                return false;
            }
            *state = ChoiceCardState::Superseded;
            dismissed = true;
            true
        }
        _ => false,
    });
    if dismissed {
        Ok(())
    } else {
        Err(refusal.to_string())
    }
}

/// Put an answered card back to pending — the queued message did not go
/// out.
pub(crate) fn unsettle_question(chat: &ChatRuntime, card_id: &str) {
    crate::provider_mode::stamp_cards(chat, |part| match part {
        MessagePart::QuestionCard {
            id, answers, state, ..
        } if id == card_id && *state == ChoiceCardState::Chosen => {
            *answers = Vec::new();
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
            questions: vec![
                CardQuestion {
                    question: "Ship the retry as prefix or suffix?".into(),
                    options: vec!["prefix".into(), "suffix".into()],
                },
                CardQuestion {
                    question: "Which store?".into(),
                    options: vec!["memory".into(), "sqlite".into()],
                },
            ],
            answers: Vec::new(),
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

    #[test]
    fn a_valid_call_reports_questions_for_the_card() {
        let result = ask_user(&json!({
            "questions": [
                { "question": "  Prefix or suffix?  ", "options": ["prefix", "suffix"] },
                { "question": "Store?", "options": [" memory ", "sqlite"] }
            ]
        }))
        .unwrap();
        assert_eq!(
            result.details,
            json!({
                "questions": [
                    { "question": "Prefix or suffix?", "options": ["prefix", "suffix"] },
                    { "question": "Store?", "options": ["memory", "sqlite"] }
                ]
            })
        );
    }

    #[test]
    fn options_keep_their_most_likely_first_order() {
        let result = ask_user(&json!({
            "questions": [
                { "question": "Store?", "options": ["sqlite", "memory", "disk"] }
            ]
        }))
        .unwrap();
        assert_eq!(
            result.details,
            json!({
                "questions": [
                    { "question": "Store?", "options": ["sqlite", "memory", "disk"] }
                ]
            })
        );
    }

    #[test]
    fn rejects_empty_thin_fat_and_duplicate_shapes() {
        assert!(ask_user(&json!({ "questions": [] })).is_err());
        let five = (0..5)
            .map(|i| json!({ "question": format!("q{i}"), "options": ["a", "b"] }))
            .collect::<Vec<_>>();
        assert!(ask_user(&json!({ "questions": five })).is_err());
        assert!(
            ask_user(&json!({ "questions": [{ "question": "", "options": ["a", "b"] }] })).is_err()
        );
        assert!(
            ask_user(&json!({ "questions": [{ "question": "q", "options": ["only"] }] })).is_err()
        );
        let seven = (0..7).map(|i| i.to_string()).collect::<Vec<_>>();
        assert!(
            ask_user(&json!({ "questions": [{ "question": "q", "options": seven }] })).is_err()
        );
        assert!(
            ask_user(&json!({ "questions": [{ "question": "q", "options": ["a", "a"] }] }))
                .is_err()
        );
    }

    #[test]
    fn tool_card_builds_only_from_an_ask_user_result() {
        let details = json!({
            "questions": [
                { "question": "q?", "options": ["a", "b"] }
            ]
        });
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
    fn settle_stamps_under_a_lock_and_refuses_bad_answer_sets() {
        let chat = chat_with(vec![card("q1", ChoiceCardState::Pending)]);
        let pairs = settle_question(&chat, "q1", vec!["  suffix  ".into(), "sqlite".into()])
            .expect("the pending card settles");
        assert_eq!(
            pairs,
            vec![
                (
                    "Ship the retry as prefix or suffix?".to_string(),
                    "suffix".to_string()
                ),
                ("Which store?".to_string(), "sqlite".to_string()),
            ]
        );
        assert!(matches!(
            &transcript(&chat)[0],
            MessagePart::QuestionCard {
                answers,
                state: ChoiceCardState::Chosen,
                ..
            } if answers.len() == 2
        ));
        assert_eq!(
            settle_question(&chat, "q1", vec!["prefix".into(), "memory".into()]).unwrap_err(),
            "this question is already settled"
        );
        assert_eq!(
            settle_question(&chat, "missing", vec!["x".into()]).unwrap_err(),
            "no such question on this chat"
        );
        // The count check runs before the stamp: a mismatched answer list
        // leaves the card answerable.
        let fresh = chat_with(vec![card("q1", ChoiceCardState::Pending)]);
        assert_eq!(
            settle_question(&fresh, "q1", vec!["only-one".into()]).unwrap_err(),
            "the answers do not match the questions"
        );
        assert_eq!(state_of(&transcript(&fresh)[0]), ChoiceCardState::Pending);
        assert!(settle_question(&chat, "q1", vec!["a".into(), String::new()]).is_err());
    }

    #[test]
    fn dismiss_retires_only_the_target_pending_card() {
        let chat = chat_with(vec![
            card("q1", ChoiceCardState::Pending),
            card("q2", ChoiceCardState::Pending),
            card("q3", ChoiceCardState::Chosen),
        ]);
        dismiss_question(&chat, "q2").expect("the pending card dismisses");
        assert_eq!(state_of(&transcript(&chat)[0]), ChoiceCardState::Pending);
        assert_eq!(state_of(&transcript(&chat)[1]), ChoiceCardState::Superseded);
        assert_eq!(state_of(&transcript(&chat)[2]), ChoiceCardState::Chosen);
        assert_eq!(dismiss_question(&chat, "q1").expect("still answerable"), ());
        dismiss_question(&chat, "q1").expect_err("a settled card refuses");
        assert_eq!(
            dismiss_question(&chat, "missing").unwrap_err(),
            "no such question on this chat"
        );
    }

    #[test]
    fn unsettle_restores_pending_for_the_retry() {
        let chat = chat_with(vec![card("q1", ChoiceCardState::Pending)]);
        settle_question(&chat, "q1", vec!["suffix".into(), "sqlite".into()]).unwrap();
        unsettle_question(&chat, "q1");
        assert!(matches!(
            &transcript(&chat)[0],
            MessagePart::QuestionCard {
                answers,
                state: ChoiceCardState::Pending,
                ..
            } if answers.is_empty()
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

    fn state_of(part: &MessagePart) -> ChoiceCardState {
        match part {
            MessagePart::QuestionCard { state, .. } => *state,
            other => panic!("expected a question card, got {other:?}"),
        }
    }

    #[test]
    fn the_notice_reads_as_the_user_answering() {
        assert_eq!(
            question_answer_notice(&[("Prefix or suffix?".into(), "suffix".into())]),
            "To your question \"Prefix or suffix?\": suffix"
        );
        assert_eq!(
            question_answer_notice(&[
                ("Prefix or suffix?".into(), "suffix".into()),
                ("Which store?".into(), "sqlite".into()),
            ]),
            "To your questions:\n- \"Prefix or suffix?\": suffix\n- \"Which store?\": sqlite"
        );
    }
}
