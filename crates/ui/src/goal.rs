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
    let marker = format!("\n\n{GOAL_BLOCK_HEADER}");
    let at = text.rfind(&marker)?;
    if !text[at + marker.len()..].trim().is_empty() {
        return None;
    }
    Some(text[..at].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
