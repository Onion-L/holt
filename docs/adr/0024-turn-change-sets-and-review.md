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

Amendment (2026-10-03): a last-message edit (ADR-0033) prunes its Turn, so
the Turn's change set is retracted with it — persisted record, in-memory
baseline, and a live-watch notification — since the card attaches to a Turn
the transcript no longer holds. The working tree keeps its edits, but the
retained work loses its review record: the replacement Turn reruns under
the same message id from a baseline that already contains it, so the
replacement reports its own set only.

## Consequences

The engine owns the baseline and durable change record. The UI renders a
change card on the Turn, opens a read-only per-file review in the right
sidebar, and opens the post-Turn file for the Open action. Accept and
non-Git snapshot support remain future work; undo is ADR-0041.
