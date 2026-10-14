# Modes: Plan and Provider

Two chat-level states that reshape what a Turn is before it runs. Plan Mode turns a chat into a planning checkpoint — a read-only exploration surface whose output is a plan the user approves. Provider Mode turns a chat into a catalog-setup session — research and resolve, with exactly the tools to propose and apply catalog changes. Both ride flags on the chat row, both mount a narrowed toolset, and both land their outcome as a card the user settles.

Not here: the permission gate that audits mutating calls (see permission-gate — orthogonal, and Plan/Provider Turns simply mount nothing that would trip it); goal mode (see goals-routines) is a loop, not a mode, and is mutually exclusive with both by rejection.

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| Plan Mode state | The chat row's `plan_mode` carries the entry permission mode, restored on approval — never moved by entry itself. Enter/Exit are idempotent; a restart restores the state without auto-starting a Turn | [`crates/engine/src/plan_mode.rs`](../../crates/engine/src/plan_mode.rs) |
| Planning Turns | The read-only exploration set (`ls`, `read`, `grep`, `read_chat`, `web_fetch`, `web_search`) plus the `submit_plan` tool; a system-prompt block names the submission channel | `plan_mode.rs` (`read_only_tool_allowed`, `planning_system_block`) |
| Plan submission | `submit_plan` carries the plan Markdown as typed data straight into the approval card — the primary channel; a complete `<proposed_plan>` block in ordinary text still folds into a card as the fallback. Each submission fully replaces the previous one | [`crates/engine/src/tools/submit_plan.rs`](../../crates/engine/src/tools/submit_plan.rs) |
| Plan resolution | Approve exits Plan Mode restoring the entry mode and enqueues an ordinary approval follow-up (the plan already lives in History, so the implementation Turn carries it naturally and starts on its own); reject keeps planning, with non-empty feedback enqueued as the next planning input; remain changes nothing but the cards. Exiting settles still-pending cards as dismissed | [`rpc/modes.rs`](../../crates/engine/src/rpc/modes.rs) |
| Provider Mode state | The chat row's `provider_mode` flag; the chat's persisted setup state under `provider-mode/<chatId>.json` — stored proposals (capped at five), the pending Key request, approved key destinations. Nothing there holds a key or secret header; a restart keeps every pending card actionable | [`crates/engine/src/provider_mode.rs`](../../crates/engine/src/provider_mode.rs) |
| Catalog-setup tools | Exactly three beyond the web tools: the read-only `model_proposal` (validates an exact catalog change against the local catalog — no-op detection, deterministic checks, an optional read-only `GET {baseUrl}/models` probe under a 15 s timeout, single-record dumps as replacement templates — then stores it and returns a proposal id) and `request_provider_key`, plus `model_apply` (see below) | [`crates/engine/src/tools/model_setup.rs`](../../crates/engine/src/tools/model_setup.rs) |
| Proposal lifecycle | `ApplyModelProposal` re-validates and applies under a baseline check narrowed to the touched providers; `DiscardModelProposal` drops it. A newer proposal on the same provider supersedes the older card; each card's state (pending, written, discarded, superseded) is stamped into the transcript | `model_setup.rs` (`apply_stored`, `baseline_fingerprint`) |
| The apply gate | `model_apply` is a forced approval: every apply asks the human gatekeeper regardless of the chat's permission mode, grants never exempt it, and the forced chip carries the proposal summary | permission-gate (`forces_approval`) |
| Key requests | A Key request lands as a card; `SettleProviderKeyRequest` saves the key to the credential store — never the chat — and queues the fixed notice that continues it; a Save also approves the exact (provider, baseUrl) destination, so later probes carry the key | [`rpc/run.rs`](../../crates/engine/src/rpc/run.rs), `model_setup.rs` |
| UI cutaways | The approval bar's plan card and the transcript's proposal / key-request cards; the composer chips a `/plan` or `/provider` directive | [`crates/ui/src/transcript/plan_card.rs`](../../crates/ui/src/transcript/plan_card.rs), [`provider_card.rs`](../../crates/ui/src/transcript/provider_card.rs) |

## Runtime path

1. **Enter** (`EnterPlanMode` / `EnterProviderMode`, idempotent). Both reject a chat carrying the other mode — or, for Provider Mode, any goal state.
2. **A Turn admitted under the flag** mounts the narrowed toolset and the mode's system block: Plan — exploration plus `submit_plan`; Provider — the web tools, `model_proposal`, `request_provider_key`, and `model_apply`. No file tools, no delegation. A switch during a running Turn takes effect from the next Turn.
3. **Landing.** A submitted plan or a stored proposal lands as a typed card in the transcript (the fallback text block folds the same way).
4. **The user settles the card.** Plan: approve / reject / remain as above. Provider: the Write button *is* the human approval — it re-validates against the current catalog (a changed baseline refuses with a stale error) and applies; Discard drops it.
5. **Exit.** Plan approval exits and restores the entry mode; Provider Mode exits by RPC. Pending cards settle as dismissed.

## Constraints, state, and failure behavior

- **No file tools in either mode** — planning explores, catalog setup proposes; neither writes the workspace.
- **A proposal is validated twice**: at creation against the catalog, and again at apply against a baseline narrowed to the touched providers. A catalog that moved underneath refuses (`stale`) rather than writing a change the user never saw.
- **Keys never enter History, a tool argument, or the transcript.** The persisted mode state carries destinations and requests, never values; the credential store is the only home for a key.
- **Mutual exclusion is rejection, not eviction**: entering one mode rejects a chat carrying the other (and Provider Mode rejects a chat with a goal), so nothing is silently cleared. Legacy `model-setup` chat rows are deleted at startup — the superseded design leaves no residue.
- **A mode switch never rewinds a running Turn** — the flag is read at admission.
- The Title task, Compaction, and subagent runs mount no mode toolset and never see these paths.

## Verification

```sh
cargo test -p holt-engine plan_mode
cargo test -p holt-engine model_setup
cargo test -p holt-engine provider_mode
cargo test -p holt-ui plan_card
```

The suites cover the read-only whitelist and prompt block, submission and replacement (tool channel and text fallback), verdict flows and card settling, proposal validation and the stale-baseline refusal, the probe's bounds, key-request settling, and the persisted mode state across a simulated restart.
