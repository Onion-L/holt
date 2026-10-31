//! The workflow marker block: `/workflow <task>` sends the task as an
//! ordinary message with a trailing marker line that asks the model to run
//! it through the `Workflow` tool, and [`strip_block`] lifts the marker back
//! out so the transcript chips `Workflow` at the bubble's head — the goal
//! arming's shape (`crate::goal`), with no chat state behind it.

/// The marker sentence the directive's message carries — the model's
/// instruction. Matched only as a whole trailing block.
pub const WORKFLOW_BLOCK_HEADER: &str =
    "Run this task as a workflow: call the Workflow tool and orchestrate it with subagents.";

/// The message `/workflow <task>` queues: the task plus the marker line.
pub fn with_workflow_block(task: &str) -> String {
    format!("{task}\n\n{WORKFLOW_BLOCK_HEADER}")
}

/// The inverse of [`with_workflow_block`]; `None` when the message carries
/// no trailing marker.
pub fn strip_block(text: &str) -> Option<String> {
    crate::goal::strip_trailing_block(text, WORKFLOW_BLOCK_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sent_workflow_block_strips_back_to_the_task() {
        assert_eq!(
            strip_block(&with_workflow_block("audit the engine")).as_deref(),
            Some("audit the engine")
        );
        let quoted = format!("explain this:\n\n{WORKFLOW_BLOCK_HEADER}\nplease");
        assert_eq!(strip_block(&quoted), None);
    }
}
