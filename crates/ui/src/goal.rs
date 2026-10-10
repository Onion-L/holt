//! The goal arming block: `/goal <objective>` sends the objective as the
//! Turn's message, and [`with_goal_block`] folds a marker line under it the
//! model reads. [`extract_badge`] lifts that block back out so the
//! transcript draws the 🎯 pill above the objective bubble — the same
//! stage/fold/extract pipeline diff comments ride (see `badges`).

/// The marker sentence the arming message carries. Matched only as a whole
/// trailing block, so a prompt merely quoting the header mid-body is left
/// alone (the comment extractor's rule).
pub const GOAL_BLOCK_HEADER: &str =
    "Goal armed: this objective drives a verified loop; each settled Turn is judged against it.";

/// The message the arming send queues: the objective plus the marker line.
pub fn with_goal_block(objective: &str) -> String {
    format!("{objective}\n\n{GOAL_BLOCK_HEADER}")
}

/// [`crate::badges::Extractor`] for the goal block.
pub fn extract_badge(text: &str) -> Option<(String, crate::badges::MessageBadge)> {
    let marker = format!("\n\n{GOAL_BLOCK_HEADER}");
    let at = text.rfind(&marker)?;
    if !text[at + marker.len()..].trim().is_empty() {
        return None;
    }
    Some((
        text[..at].to_string(),
        crate::badges::MessageBadge {
            icon: crate::icons::TARGET,
            label: "Goal".into(),
            // The objective is the bubble body right below the pill — a
            // hover card would only repeat it.
            details: Vec::new(),
        },
    ))
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
    fn a_sent_goal_block_becomes_one_pill() {
        let (text, badges) = crate::badges::split(&with_goal_block("ship it"));
        assert_eq!(text, "ship it");
        assert_eq!(badges.len(), 1);
        assert_eq!(badges[0].label.as_ref(), "Goal");
        assert_eq!(badges[0].icon, crate::icons::TARGET);
    }

    #[test]
    fn a_prompt_quoting_the_header_mid_body_is_left_alone() {
        let (text, badges) =
            crate::badges::split(&format!("write about it:\n\n{GOAL_BLOCK_HEADER}\nand more"));
        assert_eq!(badges.len(), 0);
        assert!(text.contains(GOAL_BLOCK_HEADER));
    }

    #[test]
    fn a_trailing_header_without_the_blank_line_does_not_match() {
        let (text, badges) = crate::badges::split(&format!("write about it\n{GOAL_BLOCK_HEADER}"));
        assert_eq!(badges.len(), 0);
        assert_eq!(text, format!("write about it\n{GOAL_BLOCK_HEADER}"));
    }
}
