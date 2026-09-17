# Jev review is a permission mode judged by an external decision API

## Context

The mode picker carries a display-only "Jev review" tier between
auto-review and full access. TypeSafe's System One (Jev) is a fast,
cheap decision API — typed questions over a state, answers with
calibrated probabilities and confidence — but it is not a
chat-completion model: no message interface, no generated text to
stream. The provider catalog overlays built-in chat providers only, so
no provider path can host it, and Holt ships no keys — the user brings
their own.

## Decision

Jev review is a fourth Permission mode riding the same before-tool-call
gate as auto-review. Each mutating tool call is judged by one TypeSafe
request: state is the tool identity, arguments, working directory, and
the user's latest message; a fixed set of atomic Noul questions is
combined in engine code. TypeSafe Nouls carry no separate confidence
field, so the agreed confidence gate is expressed as a dead band on the
Noul probabilities — answers between 0.4 and 0.6 escalate — plus a
dedicated ask-the-user question (action threshold 0.6). An unsure or failed judgment — retries exhausted on
429/529, network error, invalid key — escalates to the user as an
ordinary Approval. The gate never fails open and never masks an
infrastructure error as a model rejection.

The client is harness-written in the engine (reqwest + serde against
the single System One endpoint), following the web-search backend
pattern: settings-mounted, injectable for tests, never registered as a
provider and never shown in a model picker. The API key is a
device-wide, user-supplied record under the credentials pattern. With
no key the mode is unavailable — grayed in the picker, never an error —
and a chat left in the mode with no key runs its Turns under
confirm-changes silently; the mode choice itself is retained.

## Consequences

Permission mode serialization gains `jev-review`; persisted chats keep
their choice, and unknown-value fallback still lands on
confirm-changes. Usage records stamp source `Jev review`, provider
`typesafe`, model `jev-latest`. Transcript chips reuse the existing
review verdicts — the judge is identifiable only through reason text
("Jev review: …"), with no serialized chip change. The RPC surface
gains the Jev settings methods (get/save/reveal/remove), mirroring Web
search.
