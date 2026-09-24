# Provider Mode: catalog setup is a chat mode, not a dialog

ADR-0030 put catalog setup in a hidden, session-scoped `model-setup` chat
rendered inside a Settings dialog. In use it read as a chat squeezed into a
modal: a mini transcript, a separate review panel, and a key card stacked in
one dialog; a history deleted on close; and a key flow that needed a stored
proposal before the assistant could ask for a key. It is replaced by
**Provider Mode**: a chat-level state, entered in any ordinary chat, under
which the conversation itself adds and updates providers.

Provider Mode rides the chat row (`Chat::provider_mode`), orthogonal to the
permission mode and mutually exclusive with Plan Mode — entering either one
exits the other through its ordinary exit path. It is entered from the
composer's mode chip, `/provider`, or Settings' "Add with AI" (the new-chat
canvas with the mode drafted), survives restart, and stays on after a write
until the user leaves it — one chat often sets up several providers. A Turn
admitted under Provider Mode keeps the chat's workspace prompt and appends a
Provider Mode block (research → resolve → propose → stop); its toolset is
exactly `web_fetch`/`web_search`, `model_proposal`, and
`request_provider_key` — no file tools, no delegation, no MCP. Turns outside
the mode mount neither catalog tool, so the catalog-write capability stays
nil everywhere else; ADR-0030's reason for scoping it holds, with the scope
moved from a hidden chat to a mode.

A stored proposal renders as a `ModelProposal` transcript part — the change
rows plus Write / Discard, which call the unchanged `ApplyModelProposal` /
`DiscardModelProposal`. Its display state (pending, written, discarded,
superseded) is stamped into the transcript the way Plan Mode settles its
cards. A new proposal touching any provider of a pending one supersedes it,
so "make pro's context 1M" produces a fresh card and retires the old one;
proposals for unrelated providers stay independent. A Key request renders
the same way, as a `KeyRequest` part in the flow, settled through the
unchanged `SettleProviderKeyRequest`.

Because the history now lives on, the engine-side state behind the cards
persists per chat in `provider-mode/<chatId>.json` — pending proposals, the
pending Key request, and approved key destinations — and is deleted with the
chat. Without it a restart would leave cards in the history that can no
longer write. With several independent cards alive in one chat, a
whole-catalog baseline would make writing one card stale every other; the
staleness gate narrows to the providers a proposal touches: their slice of
the live settings must equal the proposal's baseline slice, the batch is
re-validated, and it applies onto the current settings under a CAS against
the snapshot read at apply time.

The key flow no longer needs a proposal first. `request_provider_key` and
`model_proposal`'s inquiry probe accept a draft provider (`id`, `name`,
`baseUrl`, `defaultApi`), so the order is resolve → key → probe → propose,
and the prompt asks for the key as soon as the docs say the endpoint needs
one instead of waiting for a 401. Draft targets keep the planned-probe
public-host gate, and the key rides only to an approved
(chat, provider, baseUrl).

Carried over unchanged: ADR-0029's invariant (only a human action writes,
and it writes the change exactly as stored) and ADR-0031's key rules (the
value never enters History, a tool argument, or the Transcript; saving
approves the destination shown). This supersedes ADR-0030 and amends
ADR-0031: the card moves into the transcript, approvals persist with the
chat instead of the session, and a draft provider can be addressed.
`ChatScope::ModelSetup`, `StartModelSetupChat`, `ListModelProposals`,
`GetProviderKeyRequest`, and the dialog's AI tab are removed; the scope
variant stays decodable and legacy rows are deleted on load.

Rejected: a form-first flow with AI autofill — it solves the dialog's
problems by bringing back the form this feature exists to avoid; keeping the
dialog and fixing its layout — the history, key-order, and modal-in-modal
problems are structural; mounting the catalog tools in every chat —
ADR-0030's prompt-injection reason; and keeping proposals in memory — cards
in a persisted history would dead-end after a restart.
