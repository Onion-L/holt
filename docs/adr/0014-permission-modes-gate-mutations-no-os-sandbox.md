# Permission modes gate mutations; no OS sandbox

Status: Accepted

## Context

Holt's agent loop mounts pi-core's write/edit/bash tools plus holt's grep,
all against the chat's real working directory, with nothing standing between
a model decision and execution. A dormant `SandboxLevel` enum
(`ReadOnly | WorkspaceWrite | DangerFullAccess`) has ridden on every
`ChatConfig` since the sync era: the UI hardcodes `WorkspaceWrite`, the
engine stores it, nothing reads it. Real OS sandboxing was evaluated
(macOS Seatbelt, Linux Landlock + seccomp) and rejected for v1: it could
only confine bash (the file tools are in-process Rust calls), its
network-blocked profiles break the cargo/npm workflows holt exists to
serve, and it would need a per-OS kernel story with ugly failure modes.

## Decision

Permission control is a per-chat **permission mode**: one gate predicate —
is this tool call mutating (`write`, `edit`, `bash`) — and three
gatekeepers. **Confirm-changes** pauses each mutating call on an Approval
card (allow once / always-allow / deny / deny with a written note).
**Auto-review** has the chat's own model judge each mutating call first;
a rejection blocks the call and returns the reason to the agent.
**Full-access** runs everything unchecked. Reads and content search are
never gated; there is no read-only tier (nothing is denied without a
gatekeeper looking at it), no command risk-classification, and no
path-based confinement — every mutating call meets the gatekeeper
regardless of target path. Always-allow grants are chat-scoped, in-memory
only (cleared on restart), matched by bash command prefix or exact file
path, and checked before the gatekeeper so they hold across mode switches.
The dormant enum is reshaped into `PermissionMode`; stored values remap
`workspace-write`/`read-only` → confirm-changes, `danger-full-access` →
full-access. New chats inherit the last used mode (first launch:
confirm-changes); the mode is snapshotted at Turn start, so switches take
effect the next Turn. The gate rides pi-core's `before_tool_call` hook —
no upstream changes.

## Consequences

- Denial (plain or with a note) settles as an error tool result and the
  Turn continues; interrupt remains the only stop-the-run channel, and it
  cancels any pending Approval.
- Auto-review costs one extra model call per mutating tool call.
- A future OS sandbox can slot beneath confirm-changes/auto-review as an
  enforcement mechanism without changing the mode vocabulary.
