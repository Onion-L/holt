//! The goal arming block: `/goal <objective>` sends the objective as the
//! Turn's message, and [`with_goal_block`] folds a marker line under it the
//! model reads. [`strip_block`] lifts that marker back out so the transcript
//! can chip the `goal` directive inline at the bubble's head — icon gutter +
//! bare name, the composer's own treatment — instead of drawing a pill above
//! the objective (see `transcript/model.rs`).

/// The marker sentence the arming message carries. Matched only as a whole
/// trailing block, so a prompt merely quoting the header mid-body is left
/// alone (the comment extractor's rule).
pub const GOAL_BLOCK_HEADER: &str =
    "Goal armed: this objective drives a verified loop; each settled Turn is judged against it.";

/// The message the arming send queues: the objective plus the marker line.
pub fn with_goal_block(objective: &str) -> String {
    format!("{objective}\n\n{GOAL_BLOCK_HEADER}")
}

/// The inverse of [`with_goal_block`]: peel the trailing marker off a sent
/// message, returning the bare objective. `None` when the message carries no
/// marker — or when the header merely appears mid-body (trailing text after
/// it), which is a quote, not an arming.
pub fn strip_block(text: &str) -> Option<String> {
    strip_trailing_block(text, GOAL_BLOCK_HEADER)
}

/// Peel a `\n\n<header>` marker off the end of a sent message — the shared
/// rule of the goal and workflow marker blocks (`crate::workflow`).
pub(crate) fn strip_trailing_block(text: &str, header: &str) -> Option<String> {
    let marker = format!("\n\n{header}");
    let at = text.rfind(&marker)?;
    if !text[at + marker.len()..].trim().is_empty() {
        return None;
    }
    Some(text[..at].to_string())
}

/// A goal-loop continuation's leading label (ADR-goal-mode-verified-loop-at-turn-settle): the transcript's
/// loop row and the queue's pending row both lead with it.
pub fn round_label(kind: &holt_proto::GoalRoundKind) -> String {
    use crate::i18n::t;
    match kind {
        holt_proto::GoalRoundKind::Continue(n) => t!("sidebar_spaces.goal.round", n = n).into(),
        holt_proto::GoalRoundKind::Resume => t!("sidebar_spaces.goal.resumed").into(),
        holt_proto::GoalRoundKind::Retry => t!("sidebar_spaces.goal.retry").into(),
    }
}

/// Where a goal loop stands right now, as the UI can see it (ADR-goal-mode-verified-loop-at-turn-settle).
/// The engine stores only `active`/`paused`/`blocked`; the moving parts —
/// a Turn running, the verifier judging, queued work, an open question —
/// are read off the queue and the transcript the UI already watches, so
/// the card and the transcript can say what the loop is doing instead of
/// sitting silent between Turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopPhase {
    /// A Turn is running (or the send is still in flight).
    Working,
    /// The last Turn settled and nothing is queued: the verifier is judging
    /// it. The engine clears the queue's active item before the pass runs.
    Checking,
    /// Work is queued ahead of the loop — the user took the wheel; the loop
    /// judges again once the queue drains.
    Queued,
    /// The Turn ended on an unanswered question card; the loop waits for
    /// the answer.
    AwaitingAnswer,
    Paused,
    Blocked,
}

/// The selected chat's loop phase, or `None` when it carries no goal.
/// `Checking` is claimed only once a reply settled after the goal was armed
/// — the arming send's window (goal set, Turn not queued yet) is not a
/// verifier pass.
pub fn loop_phase(state: &crate::state::AppState) -> Option<LoopPhase> {
    let chat = state.selected_chat_row()?;
    let goal = chat.goal.as_ref()?;
    match goal.status {
        holt_proto::GoalStatus::Paused => return Some(LoopPhase::Paused),
        holt_proto::GoalStatus::Blocked => return Some(LoopPhase::Blocked),
        holt_proto::GoalStatus::Active => {}
    }
    if state.send_pending(&chat.id, chrono::Utc::now()) {
        return Some(LoopPhase::Working);
    }
    let Some(queue) = state.message_queue.as_ref() else {
        return Some(LoopPhase::Working);
    };
    if queue.active_message_id.is_some() {
        return Some(LoopPhase::Working);
    }
    if !queue.pending.is_empty() {
        return Some(if queue.pending.iter().all(|item| item.goal_continuation) {
            LoopPhase::Working
        } else {
            LoopPhase::Queued
        });
    }
    let question_open = state.transcript.iter().flat_map(|e| &e.parts).any(|part| {
        matches!(
            part,
            holt_doc::MessagePart::QuestionCard { state, .. }
                if *state == holt_doc::parts::ChoiceCardState::Pending
        )
    });
    if question_open {
        return Some(LoopPhase::AwaitingAnswer);
    }
    let settled_since_armed = state.transcript.last().is_some_and(|last| {
        last.role != holt_doc::MessageRole::User
            && last.created_at >= goal.started_at.timestamp_millis()
    });
    Some(if settled_since_armed {
        LoopPhase::Checking
    } else {
        LoopPhase::Working
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_goal(status: &str) -> crate::state::AppState {
        let mut state = crate::state::AppState::new();
        state.chats = vec![
            serde_json::from_value(serde_json::json!({
                "id": "c", "deviceId": "d", "archived": false, "cwd": "/tmp",
                "createdAt": "2026-10-10T00:00:00Z",
                "goal": {
                    "text": "ship it", "status": status, "iteration": 1,
                    "noProgress": 0, "evalFailures": 0,
                    "startedAt": "2026-10-10T00:00:00Z"
                }
            }))
            .unwrap(),
        ];
        state.selected_chat = Some("c".into());
        state.message_queue = Some(holt_proto::MessageQueue::default());
        state
    }

    fn entry(role: holt_doc::MessageRole, created_at: i64) -> holt_doc::SessionMessageEntry {
        holt_doc::SessionMessageEntry {
            id: format!("e{created_at}"),
            role,
            parts: Vec::new(),
            created_at,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn armed_at() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-10-10T00:00:00Z")
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn the_loop_phase_reads_the_queue_and_transcript() {
        let mut state = state_with_goal("active");
        // Armed, the first Turn not queued yet: working, not a check.
        assert_eq!(loop_phase(&state), Some(LoopPhase::Working));

        // A reply settled since arming and nothing runs: the verifier.
        state.transcript = vec![
            entry(holt_doc::MessageRole::User, armed_at()),
            entry(holt_doc::MessageRole::Assistant, armed_at() + 1),
        ];
        assert_eq!(loop_phase(&state), Some(LoopPhase::Checking));

        // A Turn running.
        state.message_queue.as_mut().unwrap().active_message_id = Some("m".into());
        assert_eq!(loop_phase(&state), Some(LoopPhase::Working));
        state.message_queue.as_mut().unwrap().active_message_id = None;

        // An open question waits on the user.
        state.transcript[1].parts = vec![
            serde_json::from_value(serde_json::json!({
                "kind": "questionCard", "id": "q", "questions": [], "state": "pending"
            }))
            .unwrap(),
        ];
        assert_eq!(loop_phase(&state), Some(LoopPhase::AwaitingAnswer));

        assert_eq!(
            loop_phase(&state_with_goal("paused")),
            Some(LoopPhase::Paused)
        );
        assert_eq!(
            loop_phase(&state_with_goal("blocked")),
            Some(LoopPhase::Blocked)
        );
        let mut none = state_with_goal("active");
        none.chats[0].goal = None;
        assert_eq!(loop_phase(&none), None);
    }

    #[test]
    fn folds_the_marker_under_the_objective() {
        let out = with_goal_block("ship the login page");
        assert!(out.starts_with("ship the login page\n\n"));
        assert!(out.ends_with(GOAL_BLOCK_HEADER));
    }

    #[test]
    fn a_sent_goal_block_strips_back_to_the_objective() {
        assert_eq!(
            strip_block(&with_goal_block("ship it")).as_deref(),
            Some("ship it")
        );
    }

    #[test]
    fn a_prompt_quoting_the_header_mid_body_is_left_alone() {
        let text = format!("write about it:\n\n{GOAL_BLOCK_HEADER}\nand more");
        assert_eq!(strip_block(&text), None);
    }

    #[test]
    fn a_trailing_header_without_the_blank_line_does_not_match() {
        let text = format!("write about it\n{GOAL_BLOCK_HEADER}");
        assert_eq!(strip_block(&text), None);
    }
}
