# Turn change sets are immutable review records

## Context

Users need to understand what one agent Turn changed, separately from the
working-tree, branch, and commit diff scopes. The repository already exposes
Git diff capture and a right-sidebar diff viewer, but a Turn has its own
comparison boundary and its history must not be polluted by later edits.

## Decision

Capture a Turn baseline immediately after admission and before execution. A
Turn change set is the net Git change from that baseline to the live or final
working tree. The UI may watch the live set with the existing debounce policy;
after the Turn settles, the final set is published with the Turn result.

Persist enough per-file before/after content (or an equivalent immutable patch)
to review historical Turns after later workspace changes or restart. Keep
Turn change sets separate from CheckoutDiff objects even when they reuse the
same diff viewer. The first version is Git-only, read-only, and scoped to main
chat Turns; subagent edits belong to the parent set. Failed and interrupted
Turns retain changes already made. Net-zero files are omitted; binary files
show status without a content diff.

## Consequences

The engine owns the baseline and durable change record. The UI renders a
change card on the Turn, opens a read-only per-file review in the right
sidebar, and opens the post-Turn file for the Open action. Accept, undo, and
non-Git snapshot support remain future work.
