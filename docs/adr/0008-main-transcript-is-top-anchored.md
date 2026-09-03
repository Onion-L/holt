# The main transcript is anchored at the top, not the bottom

The main chat transcript originally used gpui's `ListAlignment::Bottom`
(chat-log mode): short content rested at the pane's bottom, and opening a
chat visibly jumped bottom → top → bottom as replay rows spliced in and the
pin machinery re-glued to the tail. We flipped it to `ListAlignment::Top`,
matching the subagent (`doc_override`) tabs: rows lay out document-style
from the top of the pane, and a short transcript leaves empty space below
rather than rising from the bottom. Chat behaviors that users keep —
pinned tail-following with the spring glide, release on scroll-up, re-stick
near the tail, jump-to-bottom, and per-chat saved viewport restore — are all
distance- or item-anchor-based and survive the flip unchanged.

## Considered options

- **Keep Bottom, only fix the load flicker**: rejected — the flicker was a
  symptom of the anchoring model, not a bug in it. The reading model for a
  coding-agent transcript is a document, and Bottom forces a whole
  glue-management layer (`is_glued`, the −0.75 px de-glue,
  `materialize_scroll_anchor`, the own-turn glue-dissolve) that exists only
  to fight gpui's Bottom-only re-glue sentinel.
- **Delete the spring/glue machinery together with the flip**: rejected —
  the `StickSpring` glide (live-follow and jump-to-bottom) is
  alignment-agnostic and is behavior we deliberately keep. Under Top the
  glue layer becomes inert dead code; removing it is deferred as a separate
  cleanup, not bundled into the behavior flip.

## Consequences

- A chat with no saved viewport still opens at its latest content:
  `scroll_to_end` plants an item anchor that Top layout materializes to a
  concrete offset on the first layout pass, before any paint — there is no
  top flash. The anchor must be planted after the splice that loads the
  rows (the existing `was_empty` branch already orders it this way).
- `SavedViewport` capture/restore, the pin/re-stick distance thresholds,
  and the jump-to-bottom pill are alignment-agnostic; they needed no
  changes in behavior.
- Two workarounds encode Bottom-specific gpui quirks and are now possibly
  redundant but harmless: `OWN_SEND_SCROLL_SLACK_PX` (viewport.rs) and the
  short-regime sizing fallback in `transcript/mod.rs`. They stay until the
  glue-layer cleanup.
