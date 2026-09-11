# Plan Mode is an independent approval checkpoint

Plan Mode is a chat state orthogonal to Permission mode. It restricts the agent to read-only exploration plus writing a versioned plan document under `.holt/plans`, requires an explicit plan submission for approval, and restores the prior Permission mode after approval; this keeps planning approval separate from the policy governing implementation changes and allows multiple plans per chat to remain auditable.

The runtime must enforce the submission boundary as well as prompt it: a Plan Mode turn cannot enter approval from ordinary text, and autonomous continuation never runs while Plan Mode is active. A chat stores one active plan reference while retaining prior plan documents.
