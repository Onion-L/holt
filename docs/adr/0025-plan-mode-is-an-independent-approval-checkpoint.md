# Plan Mode is an independent approval checkpoint

Plan Mode is a chat state orthogonal to Permission mode. It restricts the agent to read-only exploration plus writing a versioned plan document under `.holt/plans`, requires an explicit plan submission for approval, and restores the prior Permission mode after approval; this keeps planning approval separate from the policy governing implementation changes and allows multiple plans per chat to remain auditable.

The runtime must enforce the submission boundary as well as prompt it: a Plan Mode turn cannot enter approval from ordinary text, and autonomous continuation never runs while Plan Mode is active.

Amendment (2026-09-12): the submission boundary is a `<proposed_plan>` Markdown block in the assistant's ordinary text — the transcript folds each complete block into an approval card, and resolution is a chat-level verdict (approve exits Plan Mode restoring the entry permission mode; reject keeps planning with the feedback as the next planning input). The plan lives in the conversation History, so implementation reads it without injection; plan documents on disk and the write/submit tools were dropped when the design was aligned with Codex's plan mode. The enforced read-only toolset and the approval cards — the parts that make this an enforced checkpoint rather than a prompt suggestion — remain.
