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

- The inner scroll area owns wheel input while the pointer is over it.
- The outer transcript does not move during inner scrolling.
- Scrolling the outer transcript requires moving the pointer outside the inner
  scroll area.
- Nested scroll implementations should include a GPUI event-dispatch
  regression test covering both offsets.

## Evidence

The regression is covered by
`nested_compaction_scroll_does_not_move_transcript_list` in
`crates/ui/src/transcript/render.rs`.
