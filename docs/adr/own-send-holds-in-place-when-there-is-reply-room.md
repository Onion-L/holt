# Own-send holds the prompt in place when there is reply room below

Every send used to glide the prompt to a fixed top inset
(`OWN_SEND_TOP_INSET_PX`). In a short chat the prompt was already sitting a
few rows below the top with the whole viewport free under it, so the glide
moved it for no benefit — a visible flicker on every send. Now the hold
position is decided once, on the first frame the anchor's row measures
bounds, and frozen on the `OwnTurnAnchor`: if the prompt's natural top leaves
at least a third of the viewport free below the bottom chrome pad, the prompt
holds exactly where it already sits and the reply streams into the space
under it; only when the reply would not fit (prompt near the bottom edge or
below the fold) does it glide to the top inset as before.

## Considered options

- **Keep the unconditional pull to the top**: rejected — in the common
  short-chat case the motion carries no information (nothing new appears
  above the prompt) and reads as a flash. The top inset stays the fallback
  for prompts without reply room, where claiming the full viewport is
  genuinely better.
- **Install no anchor and let the ordinary bottom pin handle sends**:
  rejected — the runway reservation (a min-height for the turn that keeps the
  held layout still while the reply streams) and the wheel-release/restick
  semantics depend on the anchor; dropping it for short chats would fork the
  send path into two mechanisms with different streaming behavior.
- **Hold in place (chosen)**: one pure function `own_send_hold_inset` picks
  the inset once per anchor; everything downstream (runway sizing, the entry
  glide, the absolute hold, wheel release, restick, jump-to-bottom,
  save/restore) consumes that frozen value unchanged. Holding in place is
  just the degenerate glide whose first tick has ~zero error.

## Consequences

- The reply-room threshold is `OWN_SEND_MIN_REPLY_ROOM_FRAC` (1/3 of the
  viewport height, on top of the bottom chrome pad). It is a judgement call,
  not a measured constant; tune the single constant if it reads wrong.
- Missing bounds on the resolving frame are not "below the fold": every
  `sync` remeasures the last row (the prompt itself) and the post-layout
  step runs before the next layout re-measures it, so the anchor is often
  transiently unmeasured right after a send. Only the first step (pad not
  yet in layout) seeing the content overflow the viewport with the anchor
  unmeasured resolves to the top inset; otherwise the step installs the
  provisional pad and waits a frame for bounds.
- The hold position freezes on the anchor at first measurement. A restick
  glides back to that same position — not to the top — and a saved/restored
  viewport keeps it. Window resizes recompute the runway against the new
  viewport height but never move the hold.
- Wheel release, restick, jump-to-bottom, and chat-switch restore are
  unchanged except that the inset they re-assert is the anchor's own value
  instead of a constant.
- Consecutive sends (steer) replace the anchor; the new prompt's hold is
  measured from the layout after the old pad collapses. The clamp shift from
  the collapsing pad is pre-existing behavior, out of scope here.
