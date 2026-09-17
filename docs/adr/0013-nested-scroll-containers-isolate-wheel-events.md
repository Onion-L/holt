# Nested scroll containers isolate wheel events

Status: Accepted

## Context

GPUI scrollable `div`s and `List`s both handle `ScrollWheelEvent`.
`.overflow_y_scroll()` updates the child offset but does not contain the event.
The outer `List` listener may run first, so one wheel gesture can move both an
inner reading viewport and the transcript list. This is visible when an
expandable transcript row, such as a Compaction divider, contains a long
summary.

## Decision

Scrollable content nested inside a transcript `List` must use `.occlude()` on
the scrollable child. Do not rely on a bubble-phase `.on_scroll_wheel()` handler
to stop the outer `List`: by the time that handler runs, the outer list may
already have consumed the event.

## Consequences

- The inner scroll area owns wheel input while the pointer is over it and it
  can still move in the gesture's direction.
- The outer transcript does not move during inner scrolling.
- At the inner area's scroll boundary the gesture chains: the unabsorbed
  remainder of the wheel delta is forwarded to the transcript list
  (`forward_scroll_remainder` in `crates/ui/src/transcript/render.rs`), so the
  gesture continues instead of dead-ending. The div's built-in listener
  applies the delta (unclamped) before the bubble-phase handler runs, so the
  tracked handle's offset past its clamp IS the remainder; the handle is
  written back clamped so two wheel events in one frame cannot forward the
  same overshoot twice. Forwarding runs the same viewport bookkeeping a
  direct wheel scroll would (`Transcript::on_user_scroll`).
- Nested scroll implementations should include a GPUI event-dispatch
  regression test covering both offsets.

## Evidence

The regression is covered by
`nested_compaction_scroll_does_not_move_transcript_list` (isolation) and
`nested_scroll_chains_to_transcript_list_at_bounds` (chaining) in
`crates/ui/src/transcript/render.rs`.
